//! After an admin reset, through the server's own live sessions: machines whose histories are long push only what the
//! log lacks (not each its whole document), so the log doesn't fill again and every add arrives; and a log filled
//! with decodable junk every machine keeps, where a document's own history alone is over the cap: it stays full, and
//! every machine keeps what it has and says so.

use std::collections::VecDeque;
use std::sync::Arc;

use craft_cloud_agent::live::replica::{Peers, Replica};
use craft_cloud_agent::live::store::MemStore;
use craft_cloud_meta::{DeviceAuth, Meta, Role};
use craft_cloud_proto::live::{AppName, ClientFrame, DocKind, ServerFrame, recents_doc};
use craft_cloud_proto::local::RecentEntry;
use craft_cloud_proto::{DocId, SpaceId};
use craft_cloud_server::AppState;
use craft_cloud_server::live::Session;
use craft_cloud_store::Store;
use craft_cloud_store::memory::MemoryBackend;
use tokio::sync::broadcast;

fn entry(path: &str, title: &str, opened_ms: i64) -> RecentEntry {
    RecentEntry { node: None, path: path.into(), title: title.into(), opened_ms, saved_ms: None, machine: "pc".into() }
}

fn app() -> AppName {
    AppName::new("wordcraft").unwrap()
}

fn peers(base: u64) -> Peers {
    let mut n = 0;
    Box::new(move || {
        n += 1;
        base + n
    })
}

/// A title that doesn't compress (so a snapshot is as big as the history).
fn noise(seed: u32, len: usize) -> String {
    let mut state = u64::from(seed).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect()
}

fn key(n: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(&n.to_le_bytes());
    k[8] = 0x5a;
    k
}

struct Machine {
    replica: Replica<MemStore>,
    auth: DeviceAuth,
    conn: Option<(Session, broadcast::Receiver<DocId>)>,
    up: VecDeque<ClientFrame>,
    down: VecDeque<ServerFrame>,
    said: Vec<String>,
}

struct World {
    state: Arc<AppState>,
    space: SpaceId,
    machines: Vec<Machine>,
}

impl World {
    async fn new(cap: u64, n: usize) -> Self {
        let meta = Meta::open("sqlite::memory:").await.unwrap().with_live_doc_cap(cap);
        let user = meta.create_user("ana", "$argon2id$stub", Role::Member, 0).await.unwrap();
        let state = Arc::new(AppState::new(meta.clone(), Store::new(MemoryBackend::new()), vec![]).unwrap());
        let mut machines = Vec::new();
        for i in 0..n {
            let token = [10 + i as u8; 32];
            meta.create_device(user.id, &format!("m{i}"), &token, 0).await.unwrap();
            let auth = meta.device_by_token(&token).await.unwrap().unwrap();
            let replica = Replica::open(MemStore::default(), user.space, peers(1000 * (i as u64 + 1))).unwrap();
            machines.push(Machine { replica, auth, conn: None, up: VecDeque::new(), down: VecDeque::new(), said: Vec::new() });
        }
        Self { state, space: user.space, machines }
    }

    fn connect(&mut self, m: usize) {
        let state = self.state.clone();
        let machine = &mut self.machines[m];
        let notes = state.live.listen(machine.auth.space);
        machine.conn = Some((Session::new(state, &machine.auth), notes));
        machine.up.extend(machine.replica.connected());
        machine.up.extend(machine.replica.follow(&app()));
    }

    fn disconnect(&mut self, m: usize) {
        let machine = &mut self.machines[m];
        machine.conn = None;
        machine.up.clear();
        machine.down.clear();
        machine.replica.disconnected();
    }

    fn add(&mut self, m: usize, k: u32, path: &str, title: &str) {
        let machine = &mut self.machines[m];
        let frames = machine.replica.add(&app(), key(k), entry(path, title, i64::from(k))).unwrap();
        if machine.conn.is_some() {
            machine.up.extend(frames);
        }
    }

    /// Delivers everything both ways (and ticks the hubs when nothing moves) until `quiet` ticks in a row move nothing.
    async fn settle(&mut self, quiet: usize) {
        assert!(self.run(quiet, 100_000).await, "never settled");
    }

    /// [`World::settle`], for at most `ticks` ticks; whether it settled.
    async fn run(&mut self, quiet: usize, ticks: usize) -> bool {
        let (mut still, mut ticked) = (0, 0);
        for _ in 0..1_000_000 {
            let mut moved = false;
            for machine in &mut self.machines {
                let Some((session, notes)) = machine.conn.as_mut() else { continue };
                while let Some(frame) = machine.up.pop_front() {
                    moved = true;
                    machine.down.extend(session.handle(frame).await.frames);
                }
                while let Ok(doc) = notes.try_recv() {
                    session.note(doc);
                }
                while session.has_work() {
                    moved = true;
                    machine.down.extend(session.pump().await.unwrap());
                }
                while let Some(frame) = machine.down.pop_front() {
                    moved = true;
                    machine.up.extend(machine.replica.on_frame(frame).unwrap());
                }
                machine.replica.flush().unwrap();
                machine.said.extend(machine.replica.take_notes());
            }
            if moved {
                still = 0;
                continue;
            }
            still += 1;
            ticked += 1;
            if still > quiet {
                return true;
            }
            if ticked > ticks {
                return false;
            }
            for machine in &mut self.machines {
                if machine.conn.is_some() {
                    let frames = machine.replica.resend();
                    machine.up.extend(frames);
                }
            }
        }
        false
    }

    async fn log_bytes(&self) -> u64 {
        self.state.meta.live_doc(self.space, recents_doc(self.space, &app())).await.unwrap().map_or(0, |d| d.bytes)
    }

    fn paths(&self, m: usize) -> Vec<String> {
        self.machines[m].replica.entries(&app()).into_iter().map(|e| e.path).collect()
    }
}

/// Three machines share a long history (over a third of the cap); the admin resets the log while each
/// has an add of its own not yet sent; they come back at once. Each pushing its whole document would fill the log
/// again (three histories are over the cap) and leave adds out; pushing only what the log lacks, every add arrives.
#[test]
fn three_long_histories_after_a_reset_fit_and_every_add_arrives() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let cap = 200 * 1024;
        let mut w = World::new(cap, 3).await;
        for m in 0..3 {
            w.connect(m);
        }
        w.settle(6).await;
        let mut k = 0;
        while w.log_bytes().await < 90 * 1024 {
            for _ in 0..20 {
                k += 1;
                w.add(0, k, &format!("f{}.txt", k % 150), &noise(k, 300));
            }
            w.settle(6).await;
        }
        let history = w.log_bytes().await;
        assert!(3 * history > cap && history < cap / 2, "history {history} bytes, cap {cap}");
        for m in 0..3 {
            w.disconnect(m);
            w.add(m, 100_000 + m as u32, &format!("only-m{m}.txt"), "mine");
        }
        w.state.meta.reset_live_doc(w.space, recents_doc(w.space, &app())).await.unwrap();
        for m in 0..3 {
            w.connect(m);
        }
        let settled = w.run(12, 300).await;
        let after = w.log_bytes().await;
        assert!(after < cap, "the log filled again: {after} of {cap} bytes");
        assert!(settled, "never settled after the reset");
        for m in 0..3 {
            assert_eq!(w.paths(m), w.paths(0), "m{m} differs");
            assert_eq!(w.machines[m].replica.waiting(), 0, "m{m} still has updates to send");
            assert!(!w.machines[m].replica.is_full(&app()), "m{m} is full");
        }
        for m in 0..3 {
            assert!(w.paths(0).contains(&format!("only-m{m}.txt")), "m{m}'s add is missing: {:?}", w.paths(0));
        }
    });
}

/// A device fills the log with decodable junk that every machine keeps; after the reset the first machine's history
/// alone fills the log again (log compaction, later, is the fix). The document stays full, and nothing is lost: the
/// second machine keeps its add, shows it, and says the document is full.
#[test]
fn a_history_over_the_cap_stays_full_and_loses_nothing() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let cap = 2 * 1024 * 1024;
        let mut w = World::new(cap, 3).await;
        for m in 0..3 {
            w.connect(m);
        }
        w.settle(6).await;
        // Machine 2 plays the device that fills the log: valid Loro inserts of values the list never shows.
        let filler = loro::LoroDoc::new();
        let _ = filler.set_peer_id(4242);
        let doc = recents_doc(w.space, &app());
        let (mut session, mut client_seq) = (Session::new(w.state.clone(), &w.machines[2].auth), 0u64);
        session.handle(ClientFrame::Hello { min: 1, max: 1 }).await;
        session.handle(ClientFrame::Create { doc, kind: DocKind::Recents { app: app() } }).await;
        session.handle(ClientFrame::Subscribe { doc, after: u64::MAX, at: None }).await;
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        while w.log_bytes().await < cap {
            let vv = filler.oplog_vv();
            let junk: String = (0..60_000)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    char::from(b'a' + (state % 26) as u8)
                })
                .collect();
            filler.get_map("entries").insert(&format!("junk{client_seq}"), junk).unwrap();
            filler.commit();
            client_seq += 1;
            let bytes = filler.export(loro::ExportMode::updates(&vv)).unwrap();
            session.handle(ClientFrame::Update { doc, client_seq, bytes }).await;
        }
        drop(session);
        w.settle(6).await;
        w.disconnect(1);
        w.add(1, 7, "b-only.txt", "b");
        w.disconnect(0);
        w.state.meta.reset_live_doc(w.space, doc).await.unwrap();
        w.connect(0);
        w.connect(1);
        // A full document is tried again every so often: it never goes quiet; a few minutes of ticks.
        // (Others' operations go a part at a time, each after a random wait of up to 8 ticks.)
        w.run(1000, 1500).await;
        assert!(w.log_bytes().await >= cap, "full again: the history alone is over the cap");
        assert!(w.paths(1).contains(&"b-only.txt".to_owned()), "B's add is shown on B");
        assert!(w.machines[1].replica.waiting() > 0, "and kept to send");
        assert!(w.machines[1].said.iter().any(|s| s.contains("full")), "B says the document is full: {:?}", w.machines[1].said);
    });
}
