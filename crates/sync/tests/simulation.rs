//! Deterministic simulation: three machines share one account and do random things to their folders
//! (create, edit, delete, rename, move files and folders, hold files locked or open, make folders unlistable,
//! replace a tracked file or folder with a link and later put it back, or put a folder back, make a link on a file
//! in it and move it before any pass, keep a file "still being written" for a while), sync at random moments,
//! sometimes with
//! the connection failing mid-pass, sometimes approving held-back deletes (all of them, or some of the ones the
//! last pass listed), and sometimes crash during a pass and restart from the state saved last (after the pass before,
//! or at a checkpoint of the crashed pass).
//! Then every link is put back, everyone syncs until nothing changes, and we check:
//!
//! 1. **Convergence**: every machine ends with exactly the same files and folders.
//! 2. **Nothing the engine saw is lost**: every file content that was on a disk when a pass started is still
//!    on some disk or in the server's version history, unless a user overwrote or deleted it themselves.
//! 3. **Nothing live is deleted**: stricter, every such content is still a file on some machine's disk (history
//!    alone doesn't count), so a remote delete never wins over an edit.
//! 4. **Nothing is stuck**: after settling, the only files left out of sync are case twins of a synced name.
//!
//! It also checks after every finished pass that each node a machine tracks still has a place in its folder, and
//! that a machine never deletes on the server a node it tracked behind a link it has, or uploads a file that is
//! still being written. After every pass, finished or not, a changed file that was still being written when it
//! started is still on the disk with the same content, and not as one of the pass's conflict copies.
//!
//! The actions and the checks live in `common/sim.rs`, shared with the agent's simulation over a simulated network
//! (`apps/craft-cloud-agent/tests/network/`).
//!
//! Each seed is reproducible: `SIM_SEED=17 cargo test -p craft-cloud-sync --test simulation`. By default 40
//! seeds run; `SIM_SEEDS=1000 SIM_FIRST=2000` runs seeds 2000 to 2999 (use `--release`). Seeds run in parallel on
//! `SIM_JOBS` workers (default: the machine's parallelism, at most 8). `SIM_TRACE=1` prints every pass's report
//! and each machine's folder after every step.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use common::sim::{
    Books, Exempt, Linked, Outcome, RECOVERY_PASSES, Rng, act, before_pass, check_pass, check_places, differing, files_of, follow_rename,
    for_each_seed, hashes_of, listing, settle_and_check, side_pass,
};
use common::{Server, machine_config, restart, save_checkpoints};
use craft_cloud_proto::{FileHash, NodeId};
use craft_cloud_sync::{Fs, FsKind, MemFs, RelPath, State, rel};

async fn run(seed: u64, steps: usize) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    // MemFs clocks tick once per operation, so the real-disk racy window would hash every file. Odd seeds
    // turn the window off to exercise the size-and-time fast path (and the replaced-file check).
    let racy = if seed.is_multiple_of(2) { 2_000 } else { 0 };
    let mut ms = Vec::new();
    // What each machine's agent saved last: after its last pass, or at a checkpoint during the pass it is in (a
    // crash restarts from it).
    let mut saved: Vec<Arc<Mutex<State>>> = Vec::new();
    // m0 lands downloads with one exchange (Linux, macOS), m1 in steps (Windows: the old file moved aside, the new
    // one put in place, the old one removed; a crash can fall between them), m2 one way or the other by seed. Files
    // a program holds open (`MemFs::hold`) can't be replaced either way: the download is kept for a later pass.
    for (name, swap) in [("m0", true), ("m1", false), ("m2", seed % 4 < 2)] {
        let fs = if swap { MemFs::new() } else { MemFs::new().without_swap() };
        let mut m = machine_config(&server, &ana, name, fs, 50, |c| c.racy_window_ms = racy).await;
        saved.push(save_checkpoints(&mut m));
        ms.push(m);
    }
    let mut rng = Rng::new(seed);
    let mut seen: BTreeSet<FileHash> = BTreeSet::new();
    let mut destroyed: BTreeSet<FileHash> = BTreeSet::new();
    // A machine is recovering from a crash for at most RECOVERY_PASSES passes after its restart, less if a pass
    // finishes with nothing left for later. Conflict copies it makes meanwhile may hold content that is already at
    // the original name: the crashed pass downloaded or moved files and lost the bookkeeping, and when the user
    // then locked, moved or edited them the engine can't tell its own work from the user's. A copy is the safe
    // answer (nothing is lost), so exactly those copies (by machine, path and the content they were made with) are
    // exempt from the needless-copy check below; every other check still holds, for every file. Keyed by content
    // too, so a later copy at the same path, or an edit of the copy, is checked again.
    //
    // Copies made while the original couldn't be read (another program held it locked) are exempt the same way:
    // a file that can't be read never counts as unchanged, so a new server version of it always keeps the local
    // file as a copy. That copy only holds the same content as the new version when the server got the same
    // content twice, which takes a crash (a recovering machine uploads a file whose new version the crashed pass
    // had already committed, as an Update), and it may land on any machine where the file is locked then.
    let mut recovering: Vec<u32> = vec![0; ms.len()];
    let mut exempt: BTreeSet<Exempt> = BTreeSet::new();
    // Contents a recovering machine uploaded as new files: files it no longer recognised as tracked (the crash lost
    // that bookkeeping) go up as new nodes, so the server holds the same content twice, and another machine may
    // later keep a copy of one in the other's way. Copies of exactly that content are exempt too, and only content
    // that was already on that machine's disk when it crashed (what the crash left behind, not anything new).
    let mut reuploaded: BTreeSet<FileHash> = BTreeSet::new();
    let mut at_crash: Vec<BTreeSet<FileHash>> = vec![BTreeSet::new(); ms.len()];
    // Each machine's link, if it has one, and the held-back deletes its last finished pass listed.
    let mut links: Vec<Option<Linked>> = (0..ms.len()).map(|_| None).collect();
    let mut last_pending: Vec<Vec<NodeId>> = vec![Vec::new(); ms.len()];

    // Apps writing files for a while ("still changing") come from a stream of their own, so the histories the other
    // actions make stay the same as before it was added.
    let mut writes = Rng::new(seed ^ 0x57AB_1E5E_ED00_0001);
    // The rarer compound actions (a folder put back and moved with a link made in it) likewise.
    let mut extra = Rng::new(seed ^ 0xC04B_0ED0_0000_0002);
    // Another machine renaming the folder a held delete's link is in comes from a fourth stream: the folder each
    // machine's last pass reported a hold's link in, until another machine has renamed it.
    let mut renames = Rng::new(seed ^ 0x5E5E_0000_0000_0003);
    let mut hold_dirs: Vec<Option<RelPath>> = vec![None; ms.len()];
    // Another machine moving a file out of a folder this machine replaced with a link comes from a fifth stream.
    let mut moves = Rng::new(seed ^ 0x6060_0000_0000_0004);
    let mut link_dirs: Vec<Option<RelPath>> = vec![None; ms.len()];
    for step in 0..steps {
        // As soon as a pass reports a hold, another machine syncs, renames the link's folder (if it has it) to a fresh
        // name and syncs again, so the holder's next pass pulls the rename (fix round 5). That machine's two passes
        // run without faults, with the same checks as every other pass, and its state is saved after each.
        if let Some((k, d)) = hold_dirs.iter_mut().enumerate().find_map(|(k, d)| d.take().map(|d| (k, d))) {
            let others: Vec<usize> = (0..ms.len()).filter(|j| *j != k).collect();
            if let Some(j) = renames.pick(&others).copied()
                && let Some(mj) = ms.get_mut(j)
            {
                for again in [false, true] {
                    if again && matches!(mj.fs().stat(&d), Ok(Some(FsKind::Dir { .. }))) {
                        let to = ["r5a", "r5b", "r5c"].into_iter().map(rel).find(|to| matches!(mj.fs().stat(to), Ok(None)));
                        if let Some(to) = to
                            && mj.fs_mut().rename(&d, &to).is_ok()
                        {
                            follow_rename(&mut exempt, &d, &to);
                            if std::env::var("SIM_TRACE").is_ok() {
                                eprintln!("TRACE step {step} m{j} renamed {d} to {to} (m{k}'s held link is in it)");
                            }
                        }
                    }
                    let mut books = Books {
                        links: &mut links,
                        last_pending: &mut last_pending,
                        hold_dirs: &mut hold_dirs,
                        link_dirs: &mut link_dirs,
                        recovering: &mut recovering,
                        exempt: &mut exempt,
                        reuploaded: &mut reuploaded,
                        at_crash: &at_crash,
                    };
                    side_pass(seed, step, j, mj, &mut seen, &mut books, &saved).await;
                }
            }
        }
        // Once a pass has seen a folder a machine replaced with a link, half the time another machine moves a file
        // out of that folder to the top and syncs, so the holder's next pass pulls a server move of a node it is
        // missing (review 6's P4: the folder is later put back and moved, with a link made in it). That pass runs
        // without faults, with the same checks as every other pass.
        if let Some((k, d)) = link_dirs.iter_mut().enumerate().find_map(|(k, d)| d.take().map(|d| (k, d)))
            && moves.below(2) == 0
        {
            let others: Vec<usize> = (0..ms.len()).filter(|j| *j != k).collect();
            if let Some(j) = moves.pick(&others).copied()
                && let Some(mj) = ms.get_mut(j)
            {
                let inside: Vec<RelPath> = mj.fs().files().into_keys().filter(|p| p.starts_with(&d) && *p != d).collect();
                let to = ["r6a.txt", "r6b.txt", "r6c.txt"].into_iter().map(rel).find(|to| matches!(mj.fs().stat(to), Ok(None)));
                if let (Some(f), Some(to)) = (moves.pick(&inside).cloned(), to)
                    && mj.fs_mut().rename(&f, &to).is_ok()
                {
                    follow_rename(&mut exempt, &f, &to);
                    if std::env::var("SIM_TRACE").is_ok() {
                        eprintln!("TRACE step {step} m{j} moved {f} to {to} (m{k}'s link replaced {d})");
                    }
                    let mut books = Books {
                        links: &mut links,
                        last_pending: &mut last_pending,
                        hold_dirs: &mut hold_dirs,
                        link_dirs: &mut link_dirs,
                        recovering: &mut recovering,
                        exempt: &mut exempt,
                        reuploaded: &mut reuploaded,
                        at_crash: &at_crash,
                    };
                    side_pass(seed, step, j, mj, &mut seen, &mut books, &saved).await;
                }
            }
        }
        let i = rng.below(ms.len());
        let Some(m) = ms.get_mut(i) else { continue };
        if writes.below(10) == 0
            && let Some(f) = writes.pick(&files_of(m)).cloned()
        {
            let on = !m.fs().changing().contains(&f);
            m.fs_mut().set_changing(&f, on);
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} {f} {}", if on { "is being written" } else { "is written" });
            }
        }
        if rng.below(3) == 0 {
            seen.extend(hashes_of(m));
            // A crash: the agent dies during this pass, before it saves the state at its end. A quarter of the time the
            // pass is cut short at a random server call, a quarter of the time the machine dies after a few filesystem
            // changes (between two steps of the pass), a quarter of the time it dies right after a commit applied on
            // the server, before the answer arrives (between the commit and the checkpoint that records it), otherwise
            // the pass runs to the end. It restarts from the state saved last (at the pass's last checkpoint, or after
            // the pass before), with its folder and the server as the crashed pass left them.
            // `SIM_NO_CRASH=1` runs the same history with the crashed passes kept (no restart), to tell crash bugs apart.
            let crash_roll = rng.below(8) == 0;
            let crash = crash_roll && std::env::var("SIM_NO_CRASH").is_err();
            let how = if crash_roll { rng.below(4) } else { usize::MAX };
            if how == 3 {
                m.transport().faults.die_on_commit.store(1 + rng.below(2) as u64, Ordering::Relaxed);
            }
            let every = if how == 0 {
                2 + rng.below(12) as u64
            } else if rng.below(4) == 0 {
                2 + rng.below(6) as u64
            } else {
                0
            };
            m.transport().faults.every.store(every, Ordering::Relaxed);
            if how == 1 {
                let dead = m.transport().faults.dead.clone();
                let changes = rng.below(8) as u64;
                m.fs_mut().die_after(changes, dead);
            }
            // Held-back deletes are approved now and then: all of them, or some of the ones the last pass listed.
            match rng.below(4) {
                0 | 1 => m.approve_deletes(),
                2 => {
                    let listed = last_pending.get(i).cloned().unwrap_or_default();
                    let some: Vec<NodeId> = listed.into_iter().filter(|_| rng.below(2) == 0).collect();
                    m.approve_deletes_of(some);
                }
                _ => {}
            }
            // Now and then an app is writing a file this machine changed just as its pass runs (from the writes
            // stream), so the apply side's checks meet files still being written, not only the push's.
            if writes.below(3) == 0
                && let Some(f) = writes.pick(&differing(m)).cloned()
            {
                m.fs_mut().set_changing(&f, true);
                if std::env::var("SIM_TRACE").is_ok() {
                    eprintln!("TRACE step {step} m{i} {f} is being written (pass)");
                }
            }
            let b = before_pass(m);
            let r = m.sync_once().await;
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} sync: {r:?}\n  files {}\n  dirs {:?}", listing(m), m.fs().dirs());
            }
            let mut books = Books {
                links: &mut links,
                last_pending: &mut last_pending,
                hold_dirs: &mut hold_dirs,
                link_dirs: &mut link_dirs,
                recovering: &mut recovering,
                exempt: &mut exempt,
                reuploaded: &mut reuploaded,
                at_crash: &at_crash,
            };
            check_pass(seed, step, i, m, &r, &b, crash, &mut books);
            // Without a restart (`SIM_NO_CRASH`), a machine that "died" right after a commit applied runs on with the
            // commit's answer lost: the engine doesn't know the commit applied, so it has the same bookkeeping to redo
            // as after a crash, and is recovering the same way.
            if !crash && how == 3 && m.transport().faults.dead.load(Ordering::Relaxed) {
                if let Some(left_behind) = at_crash.get_mut(i) {
                    *left_behind = hashes_of(m);
                }
                if let Some(left) = recovering.get_mut(i) {
                    *left = RECOVERY_PASSES;
                }
            }
            m.transport().faults.every.store(0, Ordering::Relaxed);
            m.transport().faults.die_on_commit.store(0, Ordering::Relaxed);
            m.fs_mut().revive();
            m.transport().faults.dead.store(false, Ordering::Relaxed);
            if crash {
                if let Some(left_behind) = at_crash.get_mut(i) {
                    *left_behind = hashes_of(m);
                }
                let state = saved.get(i).map(|s| s.lock().unwrap().clone()).unwrap_or_default();
                let mut fresh = restart(m, state, |c| c.racy_window_ms = racy);
                if let Some(s) = saved.get_mut(i) {
                    *s = save_checkpoints(&mut fresh);
                }
                *m = fresh;
                if let Some(left) = recovering.get_mut(i) {
                    *left = RECOVERY_PASSES;
                }
                if std::env::var("SIM_TRACE").is_ok() {
                    eprintln!("TRACE step {step} m{i} crashed and restarted from its last saved state");
                }
                continue;
            }
            // The agent saves the state after every pass, finished or not.
            if let Some(s) = saved.get(i) {
                *s.lock().unwrap() = m.state().clone();
            }
            check_places(seed, step, i, m, &r);
        } else {
            let mut no_link = None;
            let link = links.get_mut(i).unwrap_or(&mut no_link);
            act(&mut rng, &mut extra, m, &mut destroyed, &mut exempt, link);
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} act\n  files {}\n  dirs {:?}", listing(m), m.fs().dirs());
            }
        }
    }

    let out = Outcome {
        links: &mut links,
        seen: &mut seen,
        destroyed: &destroyed,
        recovering: &mut recovering,
        exempt: &mut exempt,
        reuploaded: &mut reuploaded,
        at_crash: &at_crash,
    };
    settle_and_check(seed, &server, &mut ms, out).await;
}

/// Seeds run in parallel (see `for_each_seed`), each on a runtime of its own.
#[test]
fn random_histories_converge_without_losing_data() {
    for_each_seed(40, |seed| {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(run(seed, 120));
    });
}
