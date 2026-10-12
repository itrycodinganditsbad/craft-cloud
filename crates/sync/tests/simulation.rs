//! Deterministic simulation: three machines share one account and do random things to their folders
//! (create, edit, delete, rename, move files and folders, hold files locked or open, make folders unlistable),
//! sync at random moments, sometimes with the
//! connection failing mid-pass. Then everyone syncs until nothing changes, and we check:
//!
//! 1. **Convergence**: every machine ends with exactly the same files and folders.
//! 2. **Nothing the engine saw is lost**: every file content that was on a disk when a pass started is still
//!    on some disk or in the server's version history, unless a user overwrote or deleted it themselves.
//! 3. **Nothing live is deleted**: stricter, every such content is still a file on some machine's disk (history
//!    alone doesn't count), so a remote delete never wins over an edit.
//! 4. **Nothing is stuck**: after settling, the only files left out of sync are case twins of a synced name.
//!
//! It also checks after every finished pass that each node a machine tracks still has a place in its folder.
//!
//! Each seed is reproducible: `SIM_SEED=17 cargo test -p craft-cloud-sync --test simulation`. By default 40
//! seeds run; `SIM_SEEDS=1000 SIM_FIRST=2000` runs seeds 2000 to 2999 (use `--release`). `SIM_TRACE=1` prints
//! every pass's report and each machine's folder after every step.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use common::{Machine, Server, machine_config};
use craft_cloud_proto::FileHash;
use craft_cloud_sync::{Fs, FsKind, MemFs, RelPath, rel};

/// xorshift64*: tiny, deterministic, good enough to pick actions.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() { None } else { items.get(self.below(items.len())) }
    }

    fn bytes(&mut self) -> Vec<u8> {
        // Mostly small files; sometimes big enough for several chunks.
        let len = if self.below(25) == 0 { 300_000 + self.below(500_000) } else { 1 + self.below(64) };
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

const NAMES: &[&str] = &["a.txt", "b.txt", "A.TXT", "c.psd", "notes"];
const DIRS: &[&str] = &["x", "y", "x/z", "y/w"];

fn files_of(m: &Machine) -> Vec<RelPath> {
    m.fs().files().into_keys().collect()
}

fn hashes_of(m: &Machine) -> BTreeSet<FileHash> {
    m.fs().files().values().map(|d| FileHash::from_bytes(*blake3::hash(d).as_bytes())).collect()
}

/// The engine's skip reason for a name that clashes by case with one it syncs (`Engine::scan`).
fn is_case_clash(reason: &str) -> bool {
    reason.starts_with("the name clashes with ") && reason.ends_with(" when case is ignored")
}

/// One random user action on a machine's folder. Failures (target exists, parent missing) are ignored,
/// like a user's failed click.
fn act(rng: &mut Rng, m: &mut Machine, destroyed: &mut BTreeSet<FileHash>) {
    // The content at `p`, read straight from memory: a locked file is still overwritten or deleted by its user.
    // It counts as destroyed only once the user's write or delete went through.
    let old = |m: &Machine, p: &RelPath| m.fs().files().get(p).map(|d| FileHash::from_bytes(*blake3::hash(d).as_bytes()));
    let dirs: Vec<RelPath> = m.fs().dirs();
    let files = files_of(m);
    match rng.below(10) {
        0 | 1 => {
            let dir = if rng.below(3) == 0 { RelPath::root() } else { rel(rng.pick(DIRS).copied().unwrap_or("x")) };
            let name = rng.pick(NAMES).copied().unwrap_or("a.txt");
            let target = rel(&format!("{dir}/{name}"));
            let data = rng.bytes();
            if !matches!(m.fs().stat(&target), Ok(Some(FsKind::Dir { .. }))) {
                let before = old(m, &target);
                if m.fs_mut().write(&target, &data, false, 0).is_ok() {
                    destroyed.extend(before);
                }
            }
        }
        2 | 3 => {
            if let Some(f) = rng.pick(&files).cloned() {
                let data = rng.bytes();
                let before = old(m, &f);
                if m.fs_mut().write(&f, &data, false, 0).is_ok() {
                    destroyed.extend(before);
                }
            }
        }
        4 => {
            if let Some(f) = rng.pick(&files).cloned() {
                let before = old(m, &f);
                if m.fs_mut().remove_file(&f).is_ok() {
                    destroyed.extend(before);
                }
            }
        }
        5 => {
            if let Some(f) = rng.pick(&files).cloned() {
                let dir = if rng.below(2) == 0 { RelPath::root() } else { rel(rng.pick(DIRS).copied().unwrap_or("y")) };
                let name = rng.pick(NAMES).copied().unwrap_or("b.txt");
                // A rename never replaces anything (it fails when the target exists), so it destroys nothing.
                let _ = m.fs_mut().rename(&f, &rel(&format!("{dir}/{name}")));
            }
        }
        6 => {
            let dir = rel(rng.pick(DIRS).copied().unwrap_or("x"));
            let _ = m.fs_mut().mkdir(&dir);
        }
        7 => {
            if let Some(d) = rng.pick(&dirs).cloned() {
                let _ = m.fs_mut().remove_dir(&d);
            }
        }
        8 => {
            // Another program holds a file locked against reading, or open so it can't be removed, renamed or
            // replaced; or a folder can't be listed (or any of that ends).
            match rng.below(4) {
                0 | 1 => {
                    if let Some(f) = rng.pick(&files).cloned() {
                        if rng.below(2) == 0 {
                            m.fs_mut().lock(&f);
                        } else {
                            m.fs_mut().unlock(&f);
                        }
                    }
                }
                2 => {
                    let mut paths = files.clone();
                    paths.extend(dirs.iter().cloned());
                    if let Some(p) = rng.pick(&paths).cloned() {
                        if std::env::var("SIM_TRACE").is_ok() {
                            eprintln!("TRACE hold {p}");
                        }
                        m.fs_mut().hold(&p);
                    }
                }
                _ => {
                    let mut paths = files.clone();
                    paths.extend(dirs.iter().cloned());
                    if let Some(p) = rng.pick(&paths).cloned() {
                        m.fs_mut().release(&p);
                    }
                }
            }
        }
        _ => {
            if let (Some(d), Some(to)) = (rng.pick(&dirs).cloned(), rng.pick(DIRS).copied()) {
                let _ = m.fs_mut().rename(&d, &rel(to));
            }
        }
    }
}

async fn run(seed: u64, steps: usize) {
    let server = Server::new().await;
    let ana = server.user("ana").await;
    // MemFs clocks tick once per operation, so the real-disk racy window would hash every file. Odd seeds
    // turn the window off to exercise the size-and-time fast path (and the replaced-file check).
    let racy = if seed.is_multiple_of(2) { 2_000 } else { 0 };
    let mut ms = Vec::new();
    for name in ["m0", "m1", "m2"] {
        ms.push(machine_config(&server, &ana, name, MemFs::new(), 50, |c| c.racy_window_ms = racy).await);
    }
    let mut rng = Rng::new(seed);
    let mut seen: BTreeSet<FileHash> = BTreeSet::new();
    let mut destroyed: BTreeSet<FileHash> = BTreeSet::new();

    for step in 0..steps {
        let i = rng.below(ms.len());
        let Some(m) = ms.get_mut(i) else { continue };
        if rng.below(3) == 0 {
            seen.extend(hashes_of(m));
            let every = if rng.below(4) == 0 { 2 + rng.below(6) as u64 } else { 0 };
            m.transport().faults.every.store(every, Ordering::Relaxed);
            if rng.below(2) == 0 {
                m.approve_deletes();
            }
            let r = m.sync_once().await;
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} sync: {r:?}\n  files {:?}\n  dirs {:?}", m.fs().files().keys().collect::<Vec<_>>(), m.fs().dirs());
            }
            m.transport().faults.every.store(0, Ordering::Relaxed);
            // After a finished pass every node the machine tracks has a place in its folder. A node skipped for a
            // filesystem error may wait one more pass for it (its folder was moved aside, say, and it couldn't be
            // looked up), so passes that skipped one aren't checked.
            if r.as_ref().is_ok_and(|r| !r.skipped.iter().any(|(_, why)| why.ends_with("tried again next pass"))) {
                let st = m.state();
                for (id, b) in &st.base {
                    assert!(
                        st.base_path(*id).is_some(),
                        "seed {seed} step {step}: m{i} tracks {id} ({:?}) under a folder it no longer tracks",
                        b.name
                    );
                }
            }
        } else {
            act(&mut rng, m, &mut destroyed);
            if std::env::var("SIM_TRACE").is_ok() {
                eprintln!("TRACE step {step} m{i} act\n  files {:?}\n  dirs {:?}", m.fs().files().keys().collect::<Vec<_>>(), m.fs().dirs());
            }
        }
    }

    // Everyone syncs until a whole round changes nothing (deletes approved and files let go, as eventually
    // they are).
    for m in &mut ms {
        m.fs_mut().unlock_all();
        seen.extend(hashes_of(m));
    }
    let mut quiet = false;
    let mut skipped: Vec<BTreeSet<RelPath>> = vec![BTreeSet::new(); ms.len()];
    let mut last_skips: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
    for _ in 0..40 {
        let mut changed = false;
        for (i, m) in ms.iter_mut().enumerate() {
            m.approve_deletes();
            let report = m.sync_once().await.unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            changed |= report.changed() || !report.pending_deletes.is_empty();
            if let Some(s) = skipped.get_mut(i) {
                *s = report.skipped.iter().filter(|(_, why)| is_case_clash(why)).map(|(p, _)| rel(p)).collect();
            }
            last_skips.insert(i, report.skipped.clone());
        }
        if !changed {
            quiet = true;
            break;
        }
    }
    assert!(quiet, "seed {seed}: the machines never settled");

    // The only skip that may remain once everything settled (no locks, deletes approved) is a name that
    // clashes by case with a synced one; it legitimately stays local. Anything else is a stuck file.
    for (i, skips) in &last_skips {
        for (path, reason) in skips {
            assert!(is_case_clash(reason), "seed {seed}: machine {i} still skips {path} after settling: {reason}");
        }
    }

    // Case twins the engine refused to sync stay local (no other skip is left, checked above).
    let synced = |i: usize, m: &Machine| -> BTreeMap<RelPath, Vec<u8>> {
        let skip = skipped.get(i).cloned().unwrap_or_default();
        m.fs().files().into_iter().filter(|(p, _)| !skip.iter().any(|s| p.starts_with(s))).collect()
    };
    let reference = ms.first().map(|m| synced(0, m)).unwrap_or_default();
    let reference_dirs: Vec<RelPath> = ms.first().map(|m| m.fs().dirs()).unwrap_or_default();
    for (i, m) in ms.iter().enumerate() {
        let mine = synced(i, m);
        assert_eq!(mine.keys().collect::<Vec<_>>(), reference.keys().collect::<Vec<_>>(), "seed {seed}: machine {i} has different files");
        assert!(mine == reference, "seed {seed}: machine {i} has different contents");
        let skip = skipped.get(i).cloned().unwrap_or_default();
        let dirs: Vec<RelPath> = m.fs().dirs().into_iter().filter(|d| !skip.iter().any(|s| d.starts_with(s))).collect();
        let ref_dirs: Vec<RelPath> =
            reference_dirs.iter().filter(|d| !skipped.first().is_some_and(|s0| s0.iter().any(|s| d.starts_with(s)))).cloned().collect();
        assert_eq!(dirs, ref_dirs, "seed {seed}: machine {i} has different folders");
    }
    let mut on_disk: BTreeSet<FileHash> = BTreeSet::new();
    for m in &ms {
        on_disk.extend(hashes_of(m));
    }
    let mut kept: BTreeSet<FileHash> = server.history_hashes().await;
    kept.extend(on_disk.iter().copied());
    let lost: Vec<&FileHash> = seen.difference(&kept).filter(|h| !destroyed.contains(*h)).collect();
    assert!(lost.is_empty(), "seed {seed}: file versions the engine saw were lost: {lost:?}");
    // Stricter: the server's history keeps every uploaded version, so the check above can't see an engine
    // that uploads an edit and then lets a remote delete remove it everywhere. Content no user overwrote or
    // deleted must still be a file on some machine (conflict copies and case twins are files too).
    let gone: Vec<&FileHash> = seen.difference(&on_disk).filter(|h| !destroyed.contains(*h)).collect();
    assert!(gone.is_empty(), "seed {seed}: file contents no user overwrote or deleted are on no machine: {gone:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn random_histories_converge_without_losing_data() {
    if let Ok(seed) = std::env::var("SIM_SEED") {
        run(seed.parse().unwrap(), 120).await;
        return;
    }
    let seeds: u64 = std::env::var("SIM_SEEDS").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let first: u64 = std::env::var("SIM_FIRST").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    for seed in first..first + seeds {
        run(seed, 120).await;
    }
}
