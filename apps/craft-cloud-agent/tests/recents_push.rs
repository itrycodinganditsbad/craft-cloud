//! The push after a reset, through the server's own live sessions: it goes in causal order (peer by peer, a new
//! machine catching up from the log would hold over 8 MiB waiting and be stuck for good), and machines holding a
//! history none of them made don't push it in lockstep (about three times over).

use std::collections::VecDeque;
use std::sync::Arc;

use craft_cloud_agent::live::replica::{Peers, Replica};
use craft_cloud_agent::live::store::MemStore;
use craft_cloud_meta::{DeviceAuth, Meta, Role};
use craft_cloud_proto::live::{AppName, ClientFrame, ServerFrame, recents_doc};
use craft_cloud_proto::{DocId, SpaceId};
use craft_cloud_server::AppState;
use craft_cloud_server::live::Session;
use craft_cloud_store::Store;
use craft_cloud_store::memory::MemoryBackend;
use tokio::sync::broadcast;

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

fn noise(seed: u64, len: usize) -> String {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            char::from(b'a' + (state % 26) as u8)
        })
        .collect()
}

struct Machine {
    replica: Replica<MemStore>,
    auth: DeviceAuth,
    conn: Option<(Session, broadcast::Receiver<DocId>)>,
    up: VecDeque<ClientFrame>,
    down: VecDeque<ServerFrame>,
    said: Vec<String>,
    /// The most bytes of updates this machine held waiting for their history at any moment.
    most_waiting: usize,
}

struct World {
    state: Arc<AppState>,
    space: SpaceId,
    meta: Meta,
    user_id: craft_cloud_meta::UserId,
    machines: Vec<Machine>,
}

impl World {
    async fn new() -> Self {
        let meta = Meta::open("sqlite::memory:").await.unwrap();
        let user = meta.create_user("ana", "$argon2id$stub", Role::Member, 0).await.unwrap();
        let state = Arc::new(AppState::new(meta.clone(), Store::new(MemoryBackend::new()), vec![]).unwrap());
        Self { state, space: user.space, meta, user_id: user.id, machines: Vec::new() }
    }

    async fn machine(&mut self, store: MemStore) {
        let i = self.machines.len();
        let token = [10 + i as u8; 32];
        self.meta.create_device(self.user_id, &format!("m{i}"), &token, 0).await.unwrap();
        let auth = self.meta.device_by_token(&token).await.unwrap().unwrap();
        let replica = Replica::open(store, self.space, peers(1000 * (i as u64 + 1))).unwrap();
        self.machines.push(Machine { replica, auth, conn: None, up: VecDeque::new(), down: VecDeque::new(), said: Vec::new(), most_waiting: 0 });
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

    /// Delivers frames; ticks every `tick_every` rounds (a wall-clock tick), and when quiet.
    async fn run(&mut self, quiet: usize, tick_every: usize) {
        let mut still = 0;
        for round in 1..2_000_000usize {
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
                    machine.most_waiting = machine.most_waiting.max(machine.replica.waiting_bytes());
                }
                machine.replica.flush().unwrap();
                machine.said.extend(machine.replica.take_notes());
            }
            if moved {
                still = 0;
            } else {
                still += 1;
                if still > quiet {
                    return;
                }
            }
            if !moved || round % tick_every == 0 {
                for machine in &mut self.machines {
                    if machine.conn.is_some() {
                        let frames = machine.replica.resend();
                        machine.up.extend(frames);
                    }
                }
            }
        }
        panic!("never settled");
    }

    async fn log_bytes(&self) -> u64 {
        self.meta.live_doc(self.space, recents_doc(self.space, &app())).await.unwrap().map_or(0, |d| d.bytes)
    }

    fn vv(&self, m: usize) -> loro::VersionVector {
        let snap = self.machines[m].replica.snapshot_of(recents_doc(self.space, &app())).unwrap();
        let d = loro::LoroDoc::new();
        d.import(&snap).unwrap();
        d.oplog_vv()
    }
}

/// Another device (standing for a retired machine) sends a history to the log, in the updates given.
async fn feed(w: &mut World, chunks: Vec<Vec<u8>>) {
    let token = [99u8; 32];
    w.meta.create_device(w.user_id, "retired", &token, 0).await.unwrap();
    let auth = w.meta.device_by_token(&token).await.unwrap().unwrap();
    let doc = recents_doc(w.space, &app());
    let mut session = Session::new(w.state.clone(), &auth);
    session.handle(ClientFrame::Hello { min: 1, max: 1 }).await;
    session.handle(ClientFrame::Create { doc, kind: craft_cloud_proto::live::DocKind::Recents { app: app() } }).await;
    session.handle(ClientFrame::Subscribe { doc, after: u64::MAX, at: None }).await;
    for (i, bytes) in chunks.into_iter().enumerate() {
        let reply = session.handle(ClientFrame::Update { doc, client_seq: i as u64 + 1, bytes }).await;
        assert!(reply.frames.iter().all(|f| matches!(f, ServerFrame::Ack { .. })), "{:?}", reply.frames);
    }
}

/// History of peers 10 and 20 interleaved (each of 10's commits depends on 20's latest), as updates of 20 rounds.
fn history(rounds: u64, with_y: bool, size: usize) -> Vec<Vec<u8>> {
    let h = loro::LoroDoc::new();
    let mut out = Vec::new();
    let mut vv = h.oplog_vv();
    for i in 0..rounds {
        if with_y {
            h.set_peer_id(20).unwrap();
            h.get_map("entries").insert(&format!("y{i}"), "s").unwrap();
            h.commit();
        }
        h.set_peer_id(10).unwrap();
        h.get_map("entries").insert(&format!("x{i}"), noise(i + 1, size)).unwrap();
        h.commit();
        if i % 20 == 19 || i + 1 == rounds {
            out.push(h.export(loro::ExportMode::updates(&vv)).unwrap());
            vv = h.oplog_vv();
        }
    }
    out
}

/// After a reset, A pushes others' operations in causal order. Peer by peer (all of peer 10's, each waiting on peer
/// 20's, before any of peer 20's) a new machine catching up from the log would hold over 8 MiB waiting and stop taking
/// anything.
#[test]
fn a_push_in_peer_order_would_strand_a_new_machine() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut w = World::new().await;
        w.machine(MemStore::default()).await;
        feed(&mut w, history(640, true, 15_000)).await;
        w.connect(0);
        w.run(12, usize::MAX).await;
        w.disconnect(0);
        w.meta.reset_live_doc(w.space, recents_doc(w.space, &app())).await.unwrap();
        w.connect(0);
        w.run(12, usize::MAX).await;
        w.machine(MemStore::default()).await;
        w.connect(1);
        w.run(12, usize::MAX).await;
        assert_eq!(w.vv(1), w.vv(0), "the new machine never gets the history: it says {:?}", w.machines[1].said);
    });
}

/// Two peers with dependencies both ways, pushed after a reset; a fresh machine
/// catching up from the log never holds more than about one part waiting.
#[test]
fn a_fresh_machine_never_waits_on_more_than_a_part() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut w = World::new().await;
        w.machine(MemStore::default()).await;
        // Both ways: 20's commits depend on 10's last, and 10's on 20's.
        let h = loro::LoroDoc::new();
        let mut chunks = Vec::new();
        let mut vv = h.oplog_vv();
        for i in 0..200u64 {
            for peer in [20, 10] {
                h.set_peer_id(peer).unwrap();
                h.get_map("entries").insert(&format!("p{peer}-{i}"), noise(i * 3 + peer, 3_000)).unwrap();
                h.commit();
            }
            if i % 10 == 9 {
                chunks.push(h.export(loro::ExportMode::updates(&vv)).unwrap());
                vv = h.oplog_vv();
            }
        }
        feed(&mut w, chunks).await;
        w.connect(0);
        w.run(12, usize::MAX).await;
        w.disconnect(0);
        w.meta.reset_live_doc(w.space, recents_doc(w.space, &app())).await.unwrap();
        w.connect(0);
        w.run(12, usize::MAX).await;
        w.machine(MemStore::default()).await;
        w.connect(1);
        w.run(12, usize::MAX).await;
        assert_eq!(w.vv(1), w.vv(0));
        let most = w.machines[1].most_waiting;
        assert!(most <= 2 * 64 * 1024, "a fresh machine held {most} bytes waiting");
    });
}

/// Three machines hold a history none of them made (a retired machine's, or one of their own earlier runs) and come
/// back at once after a reset. Each waits a random number of ticks before each part of others' operations and looks
/// again, so they don't push it in lockstep (three times the history).
#[test]
fn others_dont_push_a_retired_history_in_lockstep() {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let mut w = World::new().await;
        for _ in 0..3 {
            w.machine(MemStore::default()).await;
        }
        feed(&mut w, history(200, false, 15_000)).await;
        for m in 0..3 {
            w.connect(m);
        }
        w.run(12, usize::MAX).await;
        let before = w.log_bytes().await;
        for m in 0..3 {
            w.disconnect(m);
        }
        w.meta.reset_live_doc(w.space, recents_doc(w.space, &app())).await.unwrap();
        for m in 0..3 {
            w.connect(m);
        }
        w.run(12, usize::MAX).await;
        let after = w.log_bytes().await;
        eprintln!("the log after the reset: {:.2}x the history", after as f64 / before as f64);
        assert!(after < before * 3 / 2, "pushed {:.2}x the history", after as f64 / before as f64);
        assert_eq!(w.vv(1), w.vv(0));
        assert_eq!(w.vv(2), w.vv(0));
    });
}
