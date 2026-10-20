//! The session against the fake agent: two machines (or apps) on one hub, a small model (an opaque title and a list).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use common::{App, Notes, NotesApp, change, open, until};
use craft_cloud_app::fake::Hub;
use craft_cloud_kit::{AgentBlobs, BlobStore, KitError, MAX_QUEUE_BYTES, Open, Session};

#[test]
fn a_file_imported_on_one_machine_is_live_on_the_other() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "notes.txt", Notes::new("Groceries", &["eggs", "milk"]));
    let mut b = open::<App>(&laptop, "notes.txt", Notes::default());
    assert_eq!(b.model().texts(), vec!["eggs", "milk"], "the laptop built its model from the live document");
    assert_eq!(*b.model().title, "Groceries");
    assert!(a.status().read_only.is_none() && b.status().read_only.is_none());
    change(&mut a, |m| m.with_title("Shopping")).unwrap();
    change(&mut b, |m| m.with_item(1, "bread")).unwrap();
    until(&mut [&mut a, &mut b], "both edits on both", |s| {
        s.iter().all(|s| *s.model().title == "Shopping" && s.model().texts() == ["eggs", "bread", "milk"])
    });
    // Unchanged parts write nothing: the same model again sends nothing.
    let before = hub.updates("notes.txt");
    let same = a.model().clone();
    a.local_change(&same, &Arc::new((*same).clone())).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    a.poll();
    assert_eq!(hub.updates("notes.txt"), before, "nothing changed, nothing sent");
}

/// A document made by a newer version of the app opens read-only: it is shown, remote changes still come, and local
/// ones (and undo) are refused; the same version and an older one are writable.
#[test]
fn a_document_from_a_newer_app_is_read_only() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut newer = open::<NotesApp<2>>(&pc, "n.txt", Notes::new("v2", &["a"]));
    let mut same = open::<NotesApp<2>>(&laptop, "n.txt", Notes::default());
    assert!(same.status().read_only.is_none(), "the same version is writable");
    let mut older = open::<NotesApp<1>>(&laptop, "n.txt", Notes::default());
    let ro = older.status().read_only.clone().unwrap();
    assert_eq!((ro.app.as_str(), ro.version), ("notes", 2));
    assert_eq!(older.model().texts(), vec!["a"], "shown");
    let refused = change(&mut older, |m| m.with_title("nope"));
    assert!(matches!(refused, Err(KitError::ReadOnly { version: 2, .. })), "{refused:?}");
    assert!(matches!(older.undo(), Err(KitError::ReadOnly { .. })));
    change(&mut newer, |m| m.with_title("from v2")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while *older.model().title != "from v2" {
        newer.poll();
        same.poll();
        older.poll();
        assert!(Instant::now() < deadline, "the newer app's change came");
        std::thread::sleep(Duration::from_millis(5));
    }
    // A document whose version is older than the app's opens writable (migrating it is the app's own change).
    let hub2 = Hub::new();
    let pc2 = hub2.machine("pc").unwrap();
    let _v1 = open::<NotesApp<1>>(&pc2, "o.txt", Notes::new("v1", &[]));
    let mut v2 = open::<NotesApp<2>>(&pc2, "o.txt", Notes::default());
    assert!(v2.status().read_only.is_none());
    change(&mut v2, |m| m.with_title("migrated")).unwrap();
}

/// Reconnecting: the agent drops the app; the model stays editable, changes queue, and go out once the session is
/// back by itself (none lost, none twice).
#[test]
fn a_dropped_connection_comes_back_and_sends_what_queued() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "r.txt", Notes::new("r", &[]));
    let mut b = open::<App>(&laptop, "r.txt", Notes::default());
    pc.restart();
    until(&mut [&mut a], "the drop is seen", |s| s.iter().all(|s| !s.status().connected));
    for i in 0..5 {
        change(&mut a, |m| m.with_item(i, &format!("offline {i}"))).unwrap();
    }
    assert_eq!(a.poll().status.queued, 5);
    pc.start();
    until(&mut [&mut a, &mut b], "back, and sent", |s| {
        s.get(1).is_some_and(|b| b.model().items.len() == 5) && s.iter().all(|s| s.status().connected)
    });
    until(&mut [&mut a, &mut b], "acked", |s| s.first().is_some_and(|a| a.status().queued == 0));
    assert_eq!(b.model().texts(), (0..5).map(|i| format!("offline {i}")).collect::<Vec<_>>());
    // A dropped connection while the agent keeps running.
    pc.drop_connections();
    change(&mut a, |m| m.with_title("after the drop")).unwrap();
    until(&mut [&mut a, &mut b], "the title came", |s| s.get(1).is_some_and(|b| *b.model().title == "after the drop"));
    let wakes = Arc::new(AtomicUsize::new(0));
    let w = wakes.clone();
    b.set_waker(Arc::new(move || {
        w.fetch_add(1, Ordering::Relaxed);
    }));
    change(&mut a, |m| m.with_title("wake up")).unwrap();
    until(&mut [&mut a, &mut b], "woken", |_| wakes.load(Ordering::Relaxed) > 0);
}

/// Restarting offline with unsent edits: the app's edits were taken by the agent (its journal) while the machine was
/// offline; the app quits; the agent restarts; the app opens the document again and sees its own edits, and they go
/// out when the machine is back online.
#[test]
fn an_app_restarted_offline_sees_its_unsent_edits() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "j.txt", Notes::new("j", &[]));
    let mut b = open::<App>(&laptop, "j.txt", Notes::default());
    pc.set_online(false);
    change(&mut a, |m| m.with_item(0, "written offline")).unwrap();
    until(&mut [&mut a], "acked by the agent", |s| s.iter().all(|s| s.status().queued == 0));
    assert_eq!(pc.waiting("j.txt"), 1);
    drop(a);
    pc.restart();
    pc.start();
    let mut again = open::<App>(&pc, "j.txt", Notes::default());
    assert_eq!(again.status().waiting, 1);
    // The agent's state, then the updates it holds that the server hasn't acked yet (taken at the first polls).
    until(&mut [&mut again], "its own unsent edit, from the agent", |s| s.iter().all(|s| s.model().texts() == ["written offline"]));
    pc.set_online(true);
    until(&mut [&mut again, &mut b], "out once online", |s| s.get(1).is_some_and(|b| b.model().texts() == ["written offline"]));
}

/// The queue's cap: past 64 MiB waiting for the agent, local changes are refused (stalled) until it is back.
#[test]
fn past_the_queue_cap_changes_are_refused_until_the_agent_is_back() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let mut a = open::<App>(&pc, "q.txt", Notes::new("q", &[]));
    pc.restart();
    until(&mut [&mut a], "the drop is seen", |s| s.iter().all(|s| !s.status().connected));
    // Big changes up to near the cap, then small ones (a few hundred bytes each), so the edge is met within one.
    let mut i = 0usize;
    let mut last_taken = 0;
    loop {
        let queued = a.poll().status.queued_bytes;
        let block = match MAX_QUEUE_BYTES - queued.min(MAX_QUEUE_BYTES) {
            left if left > 1024 * 1024 => 200 * 1024,
            left if left > 8 * 1024 => 4 * 1024,
            _ => 1,
        };
        let title = format!("{i:08}{}", "q".repeat(block));
        let result = change(&mut a, |m| m.with_title(&title));
        if queued < MAX_QUEUE_BYTES {
            assert!(result.is_ok(), "{queued} bytes queued: still taken ({result:?})");
            last_taken = queued;
        } else {
            assert!(matches!(result, Err(KitError::Stalled(_))), "{queued} bytes queued: refused");
            assert!(a.poll().status.stalled);
            assert!(queued - MAX_QUEUE_BYTES < 1024, "refused at the first change past the cap ({queued} bytes)");
            break;
        }
        i += 1;
    }
    assert!(MAX_QUEUE_BYTES - last_taken < 1024, "taken up to one small change below the cap ({last_taken} bytes)");
    pc.start();
    until(&mut [&mut a], "drained", |s| s.iter().all(|s| s.status().queued == 0 && !s.status().stalled));
    change(&mut a, |m| m.with_title("taken again")).unwrap();
}

/// Review focus 1: a remote change that comes while the app holds remote changes back (a drag, an IME composition)
/// is applied at the next poll after the gesture, never inside it; the local edit isn't lost or doubled. A change
/// from a stale model is refused and writes nothing.
#[test]
fn a_remote_change_during_a_gesture_waits_for_its_end() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "g.txt", Notes::new("g", &["one"]));
    let mut b = open::<App>(&laptop, "g.txt", Notes::default());
    a.hold_remote(true);
    let before = a.model().clone();
    change(&mut b, |m| m.with_item(1, "remote")).unwrap();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        b.poll();
        assert!(!a.poll().changed, "nothing applied during the gesture");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(Arc::ptr_eq(a.model(), &before));
    let after = Arc::new(before.with_edit(0, "one, dragged"));
    a.local_change(&before, &after).unwrap();
    a.hold_remote(false);
    until(&mut [&mut a, &mut b], "both have both", |s| s.iter().all(|s| s.model().texts() == ["one, dragged", "remote"]));
    // A stale `old` (not the session's model) writes nothing.
    let stale = before.clone();
    let refused = a.local_change(&stale, &Arc::new(stale.with_title("stale")));
    assert_eq!(refused, Err(KitError::Stale));
    assert_eq!(*a.model().title, "g");
}

#[test]
fn presence_goes_to_the_other_machine_and_fades() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "p.txt", Notes::new("p", &[]));
    let mut b = open::<App>(&laptop, "p.txt", Notes::default());
    a.set_presence(Some(b"cursor at 3".to_vec())).unwrap();
    until(&mut [&mut a, &mut b], "the laptop sees it", |s| s.get(1).is_some_and(|b| b.peers().values().any(|p| p.bytes == b"cursor at 3")));
    assert!(b.peers().contains_key(&a.peer()));
    assert!(a.peers().is_empty(), "its own isn't shown");
    assert!(a.set_presence(Some(vec![0; 4096])).is_err(), "at most 4 KiB with its header");
}

#[test]
fn blobs_go_through_the_agent_and_an_old_agent_says_so() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let (a, b) = (AgentBlobs(pc.client().blobs()), AgentBlobs(laptop.client().blobs()));
    let chunk = a.put(b"an image").unwrap();
    assert_eq!(&*b.get(&chunk).unwrap(), b"an image");
    laptop.too_old();
    let e = b.get(&chunk).unwrap_err();
    assert!(e.to_string().contains("agent_too_old"), "{e}");
    // Not running at all: the file opens as a plain file.
    pc.restart();
    let opened = Session::<App>::open(&common::agent(&pc), "x.txt", Arc::new(Notes::default()), None).unwrap();
    assert!(matches!(opened, Open::NotLive(_)));
}

/// A change the agent refuses for now (its disk) is never dropped: it and the changes after it go again, in order,
/// and the other machine ends with all of them.
#[test]
fn a_change_refused_for_now_goes_again_in_order() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "f.txt", Notes::new("f", &[]));
    let mut b = open::<App>(&laptop, "f.txt", Notes::default());
    pc.refuse_next("internal", 2);
    for i in 0..4 {
        change(&mut a, |m| m.with_item(i, &format!("item {i}"))).unwrap();
    }
    until(&mut [&mut a, &mut b], "all four on the laptop", |s| s.get(1).is_some_and(|b| b.model().items.len() == 4));
    until(&mut [&mut a, &mut b], "nothing left queued", |s| s.first().is_some_and(|a| a.status().queued == 0));
    assert!(!a.status().stopped);
    assert_eq!(b.model().texts(), (0..4).map(|i| format!("item {i}")).collect::<Vec<_>>());
}

/// A change the agent refuses for good (it doesn't decode) stops the session where the app sees it: nothing after it
/// is sent into a hole, the model stays editable, and no change is silently stranded.
#[test]
fn a_change_refused_for_good_stops_the_session() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "g.txt", Notes::new("g", &[]));
    let mut b = open::<App>(&laptop, "g.txt", Notes::default());
    pc.refuse_next("invalid", 1);
    change(&mut a, |m| m.with_item(0, "refused")).unwrap();
    until(&mut [&mut a, &mut b], "stopped", |s| s.first().is_some_and(|a| a.status().stopped));
    let error = a.status().error.clone().unwrap_or_default();
    assert!(error.contains("invalid") && error.contains("save it to the file"), "{error}");
    // Still editable; the change is kept (queued), never shown as sent.
    change(&mut a, |m| m.with_item(1, "after")).unwrap();
    assert_eq!(a.model().texts(), vec!["refused", "after"]);
    assert_eq!(a.poll().status.queued, 2, "both kept, neither dropped");
    std::thread::sleep(Duration::from_millis(100));
    b.poll();
    assert!(b.model().items.is_empty(), "nothing went out after the refusal");
}

/// A flat list of layers whose `apply_remote` fails while `fail` is set (an app's codec that can't read something).
#[derive(Clone, Debug)]
struct Flaky {
    layers: Arc<Vec<(craft_cloud_kit::ElemId, String)>>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

struct FlakyLayers<'a>(&'a Flaky);

impl craft_cloud_kit::TreeSource for FlakyLayers<'_> {
    fn roots(&self) -> Vec<craft_cloud_kit::ElemId> {
        self.0.layers.iter().map(|(id, _)| *id).collect()
    }
    fn children(&self, _: craft_cloud_kit::ElemId) -> Vec<craft_cloud_kit::ElemId> {
        Vec::new()
    }
    fn write(&self, id: craft_cloud_kit::ElemId, props: &mut craft_cloud_kit::Props<'_, '_>) -> Result<(), KitError> {
        if let Some((_, name)) = self.0.layers.iter().find(|(i, _)| *i == id) {
            props.set("name", name.as_str())?;
        }
        Ok(())
    }
}

struct FlakyApp;

impl craft_cloud_kit::LiveDocument for FlakyApp {
    type Doc = Flaky;
    const APP: &'static str = "flaky";
    const SCHEMA: u32 = 1;
    fn mappings(doc: &Flaky) -> craft_cloud_kit::Mappings<'_> {
        craft_cloud_kit::Mappings::new().tree("layers", FlakyLayers(doc))
    }
    fn apply_remote(old: &Arc<Flaky>, patch: &craft_cloud_kit::RemotePatch<'_>) -> Result<Arc<Flaky>, KitError> {
        if old.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(KitError::App("this layer doesn't read".into()));
        }
        let mut new = (**old).clone();
        if let Some(tree) = patch.tree("layers") {
            new.layers = Arc::new(tree.roots().into_iter().map(|id| (id, tree.props(id).and_then(|p| p.str("name")).unwrap_or_default())).collect());
        }
        Ok(Arc::new(new))
    }
    fn normalise(_: &mut Flaky) {}
    fn refresh_derived(_: &mut Flaky) {}
}

/// A remote change the model can't take leaves it stale: local changes are refused (never a diff from a model behind
/// the document, which would delete the other machine's new layer), and once a whole rebuild succeeds at a poll the
/// session goes on, with nothing lost.
#[test]
fn a_model_behind_the_document_takes_no_local_change_until_rebuilt() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let first = craft_cloud_kit::ElemId::new();
    let flaky = |fail: bool| Flaky { layers: Arc::new(vec![(first, "one".into())]), fail: Arc::new(std::sync::atomic::AtomicBool::new(fail)) };
    let mut a = open::<FlakyApp>(&pc, "l.txt", flaky(false));
    let mut b = open::<FlakyApp>(&laptop, "l.txt", flaky(false));
    let fail = b.model().fail.clone();
    fail.store(true, std::sync::atomic::Ordering::Relaxed);
    change(&mut a, |m| Flaky { layers: Arc::new(vec![(first, "one".into()), (craft_cloud_kit::ElemId::new(), "two".into())]), ..m.clone() }).unwrap();
    until(&mut [&mut a, &mut b], "the laptop failed to take it", |s| s.get(1).is_some_and(|b| b.status().error.is_some()));
    let refused = change(&mut b, |m| Flaky { layers: Arc::new(vec![(first, "renamed".into())]), ..m.clone() });
    assert_eq!(refused, Err(KitError::ModelStale));
    fail.store(false, std::sync::atomic::Ordering::Relaxed);
    until(&mut [&mut a, &mut b], "rebuilt", |s| s.get(1).is_some_and(|b| b.model().layers.len() == 2));
    change(&mut b, |m| {
        let mut layers = (*m.layers).clone();
        layers[0].1 = "renamed".into();
        Flaky { layers: Arc::new(layers), ..m.clone() }
    })
    .unwrap();
    until(&mut [&mut a, &mut b], "both layers on both", |s| {
        s.iter().all(|s| s.model().layers.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>() == ["renamed", "two"])
    });
}

/// The reviewer's probe: a title and layers; the layers fail to rebuild on the laptop.
#[derive(Clone, Debug)]
struct Two {
    title: Arc<String>,
    layers: Arc<Vec<(craft_cloud_kit::ElemId, String)>>,
    fail: Arc<std::sync::atomic::AtomicBool>,
}

struct TwoLayers<'a>(&'a Two);

impl craft_cloud_kit::TreeSource for TwoLayers<'_> {
    fn roots(&self) -> Vec<craft_cloud_kit::ElemId> {
        self.0.layers.iter().map(|(id, _)| *id).collect()
    }
    fn children(&self, _: craft_cloud_kit::ElemId) -> Vec<craft_cloud_kit::ElemId> {
        Vec::new()
    }
    fn write(&self, id: craft_cloud_kit::ElemId, props: &mut craft_cloud_kit::Props<'_, '_>) -> Result<(), KitError> {
        if let Some((_, name)) = self.0.layers.iter().find(|(i, _)| *i == id) {
            props.set("name", name.as_str())?;
        }
        Ok(())
    }
}

struct TwoApp;

impl craft_cloud_kit::LiveDocument for TwoApp {
    type Doc = Two;
    const APP: &'static str = "two";
    const SCHEMA: u32 = 1;
    fn mappings(doc: &Two) -> craft_cloud_kit::Mappings<'_> {
        craft_cloud_kit::Mappings::new()
            .opaque("title", craft_cloud_kit::Opaque { value: Some(&*doc.title), key: Some(Arc::as_ptr(&doc.title) as usize) })
            .tree("layers", TwoLayers(doc))
    }
    fn apply_remote(old: &Arc<Two>, patch: &craft_cloud_kit::RemotePatch<'_>) -> Result<Arc<Two>, KitError> {
        let mut new = (**old).clone();
        if let Some(title) = patch.opaque("title") {
            new.title = Arc::new(String::from_utf8_lossy(&title.unwrap_or_default()).into_owned());
        }
        if let Some(tree) = patch.tree("layers") {
            if old.fail.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(KitError::App("this layer doesn't read".into()));
            }
            new.layers = Arc::new(tree.roots().into_iter().map(|id| (id, tree.props(id).and_then(|p| p.str("name")).unwrap_or_default())).collect());
        }
        Ok(Arc::new(new))
    }
    fn normalise(_: &mut Two) {}
    fn refresh_derived(_: &mut Two) {}
}

/// A failed rebuild of one part, then another part's remote change rebuilt fine (the reviewer's probe): the model
/// stays stale (only a whole rebuild clears it), so the laptop's change from it is refused and gives no save point,
/// and the pc's new layer survives on every machine.
#[test]
fn a_part_rebuilt_later_leaves_the_model_stale() {
    use std::sync::atomic::Ordering;
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let first = craft_cloud_kit::ElemId::new();
    let two = |fail: bool| Two {
        title: Arc::new("t".into()),
        layers: Arc::new(vec![(first, "one".into())]),
        fail: Arc::new(std::sync::atomic::AtomicBool::new(fail)),
    };
    let mut a = open::<TwoApp>(&pc, "two.txt", two(false));
    let mut b = open::<TwoApp>(&laptop, "two.txt", two(false));
    let fail = b.model().fail.clone();
    fail.store(true, Ordering::Relaxed);
    change(&mut a, |m| Two { layers: Arc::new(vec![(first, "one".into()), (craft_cloud_kit::ElemId::new(), "two".into())]), ..m.clone() }).unwrap();
    until(&mut [&mut a, &mut b], "the laptop failed to take it", |s| s.get(1).is_some_and(|b| b.status().error.is_some()));
    change(&mut a, |m| Two { title: Arc::new("from pc".into()), ..m.clone() }).unwrap();
    until(&mut [&mut a, &mut b], "the title came", |s| s.get(1).is_some_and(|b| *b.model().title == "from pc"));
    assert!(b.save_point().is_err(), "still stale");
    let renamed = change(&mut b, |m| {
        let mut layers = (*m.layers).clone();
        layers[0].1 = "renamed".into();
        Two { layers: Arc::new(layers), ..m.clone() }
    });
    assert_eq!(renamed, Err(KitError::ModelStale));
    assert_eq!(b.status().queued, 0, "nothing written from the stale model");
    fail.store(false, Ordering::Relaxed);
    until(&mut [&mut a, &mut b], "rebuilt whole", |s| s.get(1).is_some_and(|b| b.model().layers.len() == 2));
    for _ in 0..50 {
        a.poll();
        b.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    let names = |s: &Session<TwoApp>| s.model().layers.iter().map(|(_, n)| n.clone()).collect::<Vec<_>>();
    assert_eq!((names(&a), names(&b)), (vec!["one".to_owned(), "two".to_owned()], vec!["one".to_owned(), "two".to_owned()]));
}

/// While the agent is down, another file takes the document's path (a live one, or one never live). Coming back,
/// the session finds a different document there: it stops with `document_replaced`, keeps its model, and neither
/// merges into the other document nor imports itself over the other file.
#[test]
fn a_path_that_is_another_document_now_is_never_merged_into() {
    for live in [true, false] {
        let hub = Hub::new();
        let pc = hub.machine("pc").unwrap();
        let mut a = open::<App>(&pc, "d.txt", Notes::new("mine", &["kept"]));
        pc.restart();
        until(&mut [&mut a], "down", |s| s.iter().all(|s| !s.status().connected));
        let other = open_other();
        hub.replace("d.txt", live.then_some(other));
        pc.start();
        a.reconnect_now();
        until(&mut [&mut a], "replaced", |s| s.iter().all(|s| s.status().replaced));
        let status = a.poll().status;
        assert!(status.stopped && !status.connected);
        assert!(status.error.unwrap_or_default().contains("document_replaced"));
        assert_eq!(*a.model().title, "mine", "the model is kept");
        change(&mut a, |m| m.with_title("still editable")).unwrap();
        // The other file's document has nothing of this one.
        assert_eq!(hub.updates("d.txt"), 0, "live {live}: nothing was sent into it");
        let mut b = open::<App>(&pc, "d.txt", Notes::default());
        assert_eq!(*b.model().title, if live { "other" } else { "" }, "live {live}");
        b.poll();
    }
}

/// A snapshot of another document (title "other").
fn open_other() -> Vec<u8> {
    let mut r = craft_cloud_kit::raw::Replica::default();
    let mut schema = std::collections::HashMap::new();
    schema.insert("app".to_owned(), loro::LoroValue::from("notes"));
    schema.insert("version".to_owned(), loro::LoroValue::from(1i64));
    r.doc.get_map("root").insert("schema", loro::LoroValue::from(schema)).unwrap();
    r.write::<App>(None, &Notes::new("other", &[])).unwrap();
    r.doc.export(loro::ExportMode::Snapshot).unwrap()
}

/// A session opened off the UI thread can be handed to it.
/// Every session writes with a fresh peer of its own (the reviewer's reuse probe, with sessions): a window with
/// changes unsent while the agent was away, and a new window opened after the agent came back that writes more, end
/// with both windows' changes on every machine, and the first window's later edits too. And however many sessions
/// have edited a document, a save record names one or two heads.
#[test]
fn every_session_has_a_peer_of_its_own_and_no_change_is_lost() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "x.txt", Notes::new("t", &["base"]));
    until(&mut [&mut a], "acked", |s| s[0].status().queued == 0);
    pc.restart();
    until(&mut [&mut a], "the drop is seen", |s| !s[0].status().connected);
    for i in 0..3 {
        change(&mut a, |m| m.with_item(0, &format!("A{i}"))).unwrap();
    }
    pc.start();
    // Another window, opened before the first is back (the first open after an agent restart), writes ten.
    let mut c = open::<App>(&pc, "x.txt", Notes::default());
    assert_ne!(c.peer(), a.peer());
    for i in 0..10 {
        change(&mut c, |m| m.with_item(0, &format!("C{i}"))).unwrap();
    }
    until(&mut [&mut c], "C's changes are in", |s| s[0].status().queued == 0);
    let mut b = open::<App>(&laptop, "x.txt", Notes::default());
    let all = |s: &Session<App>| {
        (0..3).all(|i| s.model().texts().contains(&format!("A{i}"))) && (0..10).all(|i| s.model().texts().contains(&format!("C{i}")))
    };
    until(&mut [&mut a, &mut b, &mut c], "every change everywhere", |s| s.iter().all(|s| all(s) && s.status().queued == 0));
    assert!(!a.status().stopped && a.status().error.is_none(), "{:?}", a.status());
    change(&mut a, |m| m.with_item(0, "A-after")).unwrap();
    until(&mut [&mut a, &mut b], "a's later edit too", |s| s[1].model().texts().contains(&"A-after".to_owned()));
    for round in 0..20 {
        let fake = if round % 2 == 0 { &pc } else { &laptop };
        let mut s = open::<App>(fake, "x.txt", Notes::default());
        change(&mut s, |m| m.with_item(0, &format!("round {round}"))).unwrap();
        until(&mut [&mut s], "acked", |s| s.iter().all(|s| s.status().queued == 0));
    }
    until(&mut [&mut a], "every round", |s| s[0].model().texts().contains(&"round 19".to_owned()));
    let point = a.save_point().unwrap();
    assert!((1..=2).contains(&point.frontiers().len()), "{:?}", point.frontiers());
}

#[test]
fn a_session_can_move_to_another_thread() {
    fn send<T: Send>() {}
    send::<Session<App>>();
}

fn open_with_bytes(fake: &craft_cloud_app::fake::Fake, path: &str, file: Notes, bytes: &[u8]) -> Session<App> {
    match Session::<App>::open(&common::agent(fake), path, Arc::new(file), Some(bytes)).unwrap() {
        Open::Live(s) => *s,
        Open::NotLive(why) => panic!("not live: {why:?}"),
    }
}

/// Saving tells the agent (content hash and version) and marks the document; a machine that opens the file with
/// content a Craft app saved (or imported) sees nothing changed outside, one with other content does.
#[test]
fn saves_reach_the_agent_and_a_file_changed_outside_is_reported() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open_with_bytes(&pc, "s.txt", Notes::new("s", &["x"]), b"as read at import");
    assert!(!a.status().changed_outside, "the import's own bytes");
    change(&mut a, |m| m.with_title("saved")).unwrap();
    let record = save(&mut a, b"as saved");
    assert_eq!(record.hash, *blake3::hash(b"as saved").as_bytes());
    assert!(record.frontiers.iter().any(|(p, _)| *p == a.peer()), "this session's edit is its head");
    until(&mut [&mut a], "the agent has the record", |_| pc.saves().iter().any(|(h, _)| *h == craft_cloud_api::local::blobs::hex(&record.hash)));
    until(&mut [&mut a], "sent", |s| s.iter().all(|s| s.status().queued == 0));
    // (The save mark may come with the agent's first updates after the state: looked at again as they come.)
    let mut same = open_with_bytes(&laptop, "s.txt", Notes::default(), b"as saved");
    until(&mut [&mut same], "the save is known", |s| s.iter().all(|s| *s.model().title == "saved" && !s.status().changed_outside));
    let mut imported = open_with_bytes(&laptop, "s.txt", Notes::default(), b"as read at import");
    until(&mut [&mut imported], "the import is known", |s| s.iter().all(|s| *s.model().title == "saved" && !s.status().changed_outside));
    let mut other = open_with_bytes(&laptop, "s.txt", Notes::default(), b"edited in another program");
    until(&mut [&mut other], "caught up", |s| s.iter().all(|s| *s.model().title == "saved"));
    assert!(other.status().changed_outside);
    // Saving clears it.
    save(&mut other, b"saved from the live document");
    assert!(!other.status().changed_outside);
}

/// Whether the file was changed outside Craft is decided once, at the open, from the agent's records, whatever the
/// number of saves: a reader that opened the import's bytes stays "not changed" through 40 saves on another machine
/// (the reviewer's probe flipped at the 33rd), a window that opens the first of those saves' bytes sees no outside
/// edit, and one that opens other bytes does, and keeps saying so while others save, until it saves itself.
#[test]
fn changed_outside_is_decided_at_the_open_by_the_records() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let imported = b"file as read at the import".to_vec();
    let mut a = open_with_bytes(&pc, "n.txt", Notes::new("t", &["a"]), &imported);
    let mut b = open_with_bytes(&laptop, "n.txt", Notes::default(), &imported);
    assert!(!a.status().changed_outside && !b.status().changed_outside, "the import's own bytes");
    for i in 0..40 {
        change(&mut a, |m| m.with_title(&format!("t{i}"))).unwrap();
        save(&mut a, format!("save {i}").as_bytes());
    }
    until(&mut [&mut a, &mut b], "every save is known", |s| *s[1].model().title == "t39" && pc.saves().len() >= 40);
    assert!(!b.status().changed_outside, "a reader is never told its Craft file was edited outside");
    let first = open_with_bytes(&laptop, "n.txt", Notes::default(), b"save 0");
    assert!(!first.status().changed_outside, "the oldest of 40 saves is still a Craft save");
    let mut edited = open_with_bytes(&laptop, "n.txt", Notes::default(), b"edited in another program");
    assert!(edited.status().changed_outside);
    change(&mut a, |m| m.with_title("t40")).unwrap();
    save(&mut a, b"save 40");
    until(&mut [&mut a, &mut edited], "the next save came", |s| *s[1].model().title == "t40");
    assert!(edited.status().changed_outside, "decided at the open");
    save(&mut edited, b"saved from the live document");
    assert!(!edited.status().changed_outside);
}

/// "Bring it in" is the user's choice, never the kit's: the session only says the file changed outside. Taken, the
/// file's content replaces the live content as one ordinary change, named "Replace with the file's content", that
/// reaches the other machine; undo brings the live edits back, everywhere.
#[test]
fn bringing_the_file_in_is_one_named_undoable_change() {
    use craft_cloud_kit::REPLACE_WITH_FILE;
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open_with_bytes(&pc, "in.txt", Notes::new("live", &["kept live"]), b"imported");
    let mut b = open_with_bytes(&laptop, "in.txt", Notes::default(), b"edited in another program");
    assert!(b.status().changed_outside);
    until(&mut [&mut a, &mut b], "both live", |s| s.iter().all(|s| *s.model().title == "live"));
    for _ in 0..20 {
        b.poll();
    }
    assert_eq!(*b.model().title, "live", "nothing replaced by itself");
    b.replace_with_file(Notes::new("from the file", &["the editor's line"])).unwrap();
    assert!(!b.status().changed_outside);
    assert_eq!(b.step_name(true), Some(REPLACE_WITH_FILE));
    until(&mut [&mut a, &mut b], "replaced everywhere", |s| s.iter().all(|s| s.model().texts() == ["the editor's line"]));
    assert!(b.undo().unwrap());
    until(&mut [&mut a, &mut b], "the live edits back everywhere", |s| {
        s.iter().all(|s| *s.model().title == "live" && s.model().texts() == ["kept live"])
    });
    assert_eq!(b.step_name(false), Some(REPLACE_WITH_FILE), "redo names it too");
}

/// Bringing in a file whose content is the live content already makes no step, so it names none: the next ordinary
/// edit's undo isn't called "Replace with the file's content".
#[test]
fn a_name_never_sticks_to_the_next_ordinary_edit() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let mut a = open_with_bytes(&pc, "same.txt", Notes::new("same", &["one"]), b"imported");
    let same = (**a.model()).clone();
    a.replace_with_file(same).unwrap();
    assert_eq!(a.step_name(true), None);
    change(&mut a, |m| m.with_item(1, "an ordinary edit")).unwrap();
    assert!(a.can_undo());
    assert_eq!(a.step_name(true), None, "the edit's undo has no name");
}

/// An app's autosave: when dirty or the file is behind, a save (with a record when it can). True if it saved.
fn autosave(s: &mut Session<App>, dirty: &mut bool, n: &mut usize) -> bool {
    if !(*dirty || s.file_behind()) {
        return false;
    }
    let Ok(point) = s.save_point() else { return false };
    s.record(&point, format!("{:?}", point.model()).as_bytes());
    *dirty = false;
    *n += 1;
    true
}

/// Two machines with the document open, both autosaving when dirty or when the file is behind: concurrent saves
/// (the newer upload wins, the other's edits not in it) and a save made offline that goes up later over the other's
/// both end with a file that has every edit, and then nobody saves again (no loop).
#[test]
fn the_file_catches_up_with_every_edit_and_saving_stops() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "f.txt", Notes::new("f", &[]));
    let mut b = open::<App>(&laptop, "f.txt", Notes::default());
    let (mut dirty_a, mut dirty_b, mut saves) = (false, false, 0usize);
    let vv = |s: &Session<App>| s.save_point().unwrap().frontiers().to_vec();
    // Settles: both poll and autosave until the file has both documents' versions and nobody saved for a while.
    // (A catch-up save may wait out the backstop, `CATCH_UP_EVERY`, after the last one.)
    let settle = |a: &mut Session<App>, b: &mut Session<App>, da: &mut bool, db: &mut bool, saves: &mut usize, what: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut quiet = 0;
        while quiet < 60 || hub.file("f.txt").is_some_and(|f| f != vv(a) || vv(a) != vv(b)) {
            a.poll();
            b.poll();
            let saved = autosave(a, da, saves) | autosave(b, db, saves);
            quiet = if saved { 0 } else { quiet + 1 };
            assert!(Instant::now() < deadline, "never settled: {what} ({saves} saves)");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    // Concurrent: both edit, then both save before either has the other's edit.
    change(&mut a, |m| m.with_item(0, "from the pc")).unwrap();
    change(&mut b, |m| m.with_item(0, "from the laptop")).unwrap();
    autosave(&mut a, &mut true, &mut saves);
    autosave(&mut b, &mut true, &mut saves);
    let before = saves;
    settle(&mut a, &mut b, &mut dirty_a, &mut dirty_b, &mut saves, "concurrent saves");
    assert_eq!(a.model().items.len(), 2);
    assert_eq!(hub.file("f.txt"), Some(vv(&a)), "the file has every edit");
    assert_eq!(vv(&a), vv(&b));
    assert!(saves - before <= 4, "{} saves to catch up", saves - before);
    // Offline: the laptop edits and saves while the pc does too; the laptop's save goes up last, without the pc's edit.
    laptop.set_online(false);
    until(&mut [&mut b], "offline", |s| !s[0].status().online);
    change(&mut b, |m| m.with_title("laptop, offline")).unwrap();
    autosave(&mut b, &mut true, &mut saves);
    change(&mut a, |m| m.with_item(0, "pc, meanwhile")).unwrap();
    autosave(&mut a, &mut true, &mut saves);
    until(&mut [&mut a], "the pc's save is the file", |s| hub.file("f.txt") == Some(vv(s[0])));
    laptop.set_online(true);
    let before = saves;
    settle(&mut a, &mut b, &mut dirty_a, &mut dirty_b, &mut saves, "a save from offline");
    assert_eq!(*b.model().title, "laptop, offline");
    assert_eq!(a.model().items.len(), 3);
    assert_eq!(hub.file("f.txt"), Some(vv(&a)), "the file has every edit");
    assert_eq!(vv(&a), vv(&b));
    assert!(saves - before <= 4, "{} saves to catch up", saves - before);
    // An edit outside Craft is never written over by itself.
    hub.edit_outside("f.txt");
    until(&mut [&mut a, &mut b], "heard", |s| s.iter().all(|s| !s.file_behind()));
    let before = saves;
    settle(&mut a, &mut b, &mut dirty_a, &mut dirty_b, &mut saves, "an outside edit");
    assert_eq!(saves, before);
}

/// The live relay is down while the file sync goes on (the reviewer's probe): each machine saved its own edit, and
/// each file record then has the other's edit, which its live document lacks. Neither is behind: each waits for the
/// live updates, so nobody saves (was 566 saves in 3 s). Once live is back, only the side that is then ahead saves,
/// once, and the file has both edits.
#[test]
fn with_live_down_and_files_syncing_nobody_saves_in_a_loop() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "l.txt", Notes::new("l", &[]));
    let mut b = open::<App>(&laptop, "l.txt", Notes::default());
    until(&mut [&mut a, &mut b], "acked", |s| s.iter().all(|s| s.status().queued == 0));
    hub.set_live(false);
    change(&mut a, |m| m.with_item(0, "pc")).unwrap();
    change(&mut b, |m| m.with_item(0, "laptop")).unwrap();
    let (mut da, mut db) = (true, true);
    let (mut saves_a, mut saves_b) = (0usize, 0usize);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        a.poll();
        b.poll();
        autosave(&mut a, &mut da, &mut saves_a);
        autosave(&mut b, &mut db, &mut saves_b);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(saves_a <= 2 && saves_b <= 2, "{saves_a} and {saves_b} saves: the edit's, and at most one more each");
    let before = saves_a + saves_b;
    hub.set_live(true);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        a.poll();
        b.poll();
        autosave(&mut a, &mut da, &mut saves_a);
        autosave(&mut b, &mut db, &mut saves_b);
        let both = |s: &Session<App>| s.model().items.len() == 2;
        if both(&a) && both(&b) && hub.file("l.txt").as_deref() == Some(a.save_point().unwrap().frontiers()) {
            break;
        }
        assert!(Instant::now() < deadline, "never caught up");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Both may be ahead of the file once they have each other's edit (the same content then): at most one each.
    assert!(saves_a + saves_b - before <= 2, "{} saves once live was back", saves_a + saves_b - before);
}

/// A file ahead of the live document (another machine's save came by file sync, its live updates not yet) is never
/// written over by itself: `file_behind` says no until the live document has everything the file has.
#[test]
fn a_file_ahead_of_the_live_document_is_never_written_over() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "ahead.txt", Notes::new("a", &[]));
    let mut b = open::<App>(&laptop, "ahead.txt", Notes::default());
    until(&mut [&mut a, &mut b], "acked", |s| s.iter().all(|s| s.status().queued == 0));
    hub.set_live(false);
    change(&mut a, |m| m.with_item(0, "only on the pc yet")).unwrap();
    save(&mut a, b"the pc's save");
    for _ in 0..50 {
        a.poll();
        b.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(hub.file("ahead.txt").is_some());
    assert!(!b.file_behind(), "the file is ahead of the laptop's live document");
    hub.set_live(true);
    until(&mut [&mut a, &mut b], "the edit came live", |s| s[1].model().items.len() == 1);
    assert!(!b.file_behind(), "now the same: nothing to save");
    // An edit on the laptop makes it the one ahead.
    change(&mut b, |m| m.with_item(0, "the laptop's")).unwrap();
    assert!(b.file_behind());
}

/// A save as an app makes it: the save point, then the record (before the file is written).
fn save<D: craft_cloud_kit::LiveDocument>(s: &mut Session<D>, bytes: &[u8]) -> craft_cloud_kit::SaveRecord {
    let point = s.save_point().unwrap();
    s.record(&point, bytes)
}

/// A save point says which model the bytes come from and the version it matches; there is none (and so no record)
/// when the document is read-only here (the reviewer's probe), stopped, or the model is behind the live document.
#[test]
fn a_save_point_is_refused_when_a_record_would_be_wrong() {
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let _newer = open::<NotesApp<2>>(&pc, "r.txt", Notes::new("v2", &["a"]));
    let mut older = open::<NotesApp<1>>(&laptop, "r.txt", Notes::default());
    assert!(older.status().read_only.is_some());
    assert!(matches!(older.save_point(), Err(KitError::NoRecord(_))), "read-only");
    for _ in 0..20 {
        older.poll();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(laptop.saves().is_empty(), "no record from a read-only document");
    // Stopped: a change refused for good.
    let mut a = open::<App>(&pc, "q.txt", Notes::new("q", &[]));
    let point = a.save_point().unwrap();
    assert_eq!(**point.model(), **a.model());
    assert!(!point.frontiers().is_empty(), "the import's head");
    pc.refuse_next("invalid", 1);
    change(&mut a, |m| m.with_title("refused")).unwrap();
    until(&mut [&mut a], "stopped", |s| s.iter().all(|s| s.status().stopped));
    assert!(matches!(a.save_point(), Err(KitError::NoRecord(_))), "stopped");
}

/// The record goes to the agent before the file is written, and one made while the agent is away is kept and goes
/// once it is back (the agent then adds it to the version it uploaded meanwhile).
#[test]
fn a_record_made_while_the_agent_is_away_goes_once_it_is_back() {
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let mut a = open::<App>(&pc, "w.txt", Notes::new("w", &[]));
    let hex = |b: &[u8]| craft_cloud_api::local::blobs::hex(blake3::hash(b).as_bytes());
    save(&mut a, b"saved with the agent there");
    until(&mut [&mut a], "told at once", |_| pc.saves().iter().any(|(h, _)| *h == hex(b"saved with the agent there")));
    pc.restart();
    until(&mut [&mut a], "the agent went", |s| s.iter().all(|s| !s.status().connected));
    change(&mut a, |m| m.with_title("offline")).unwrap();
    let record = save(&mut a, b"saved while the agent was away");
    assert!(record.frontiers.iter().any(|(p, _)| *p == a.peer()));
    assert!(!pc.saves().iter().any(|(h, _)| *h == hex(b"saved while the agent was away")));
    pc.start();
    until(&mut [&mut a], "the record went once the agent was back", |_| {
        pc.saves().iter().any(|(h, v)| *h == hex(b"saved while the agent was away") && *v == record.frontiers)
    });
}
