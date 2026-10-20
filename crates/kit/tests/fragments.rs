//! Changes over the agent's 1 MiB frame limit: one change is one update, which goes in fragments, and no machine
//! ever holds part of one. What the other machine does meanwhile (a local edit of the same rows, node or story, an
//! undo, a save) never sees, repairs or writes back part of it; a set that stops coming holds nothing up and comes
//! whole through the next catch-up; a sender or receiver that stops part of the way loses nothing.

mod common;
mod model;

use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use common::{App, Notes, change, open, until};
use craft_cloud_api::fragments::split;
use craft_cloud_api::limits::MAX_UPDATE_BYTES;
use craft_cloud_app::fake::{FragmentFaults, Hub};
use craft_cloud_kit::{ElemId, RichText, Session};
use model::{Doc, Node, TestApp};

/// Fragments after a set's first never reach the apps (the set comes part of the way, and stops).
const FIRST_ONLY: FragmentFaults = FragmentFaults { seed: 3, drop: 1000, delay: 0, max_delay: 0 };
/// How long a set that stopped coming is kept, for every test here (the process's one setting).
const PATIENCE: Duration = Duration::from_millis(1500);

fn setup() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        craft_cloud_app::live::set_fragment_patience_for_tests(PATIENCE);
        // Frames of 64 KiB: changes of a few hundred KB go in fragments as changes over 1 MiB do.
        craft_cloud_app::live::set_frame_limit_for_tests(64 * 1024);
    });
}

/// Six rows of 200 KiB each, added at the top: about 1.2 MB, more than a frame.
fn big_rows(m: &Notes) -> Notes {
    let mut big = m.clone();
    for i in 0..6 {
        big = big.with_item(i, &format!("{i}{}", "x".repeat(200 * 1024)));
    }
    Notes { hint: None, ..big }
}

/// Polls `sessions` for `wait`, checking `never` holds at every poll.
fn poll_for<D: craft_cloud_kit::LiveDocument>(sessions: &mut [&mut Session<D>], wait: Duration, never: impl Fn(&[&mut Session<D>]) -> bool) {
    let end = Instant::now() + wait;
    while Instant::now() < end {
        for s in sessions.iter_mut() {
            s.poll();
        }
        assert!(never(sessions), "a model held part of a change");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// One change is one commit and one update, whatever its size; past 1 MiB it is sent as fragments that fit a frame.
#[test]
fn a_big_change_is_one_update_and_goes_in_fragments() {
    let mut r = craft_cloud_kit::raw::Replica::default();
    let base = Notes::new("t", &["first"]);
    r.write::<App>(None, &base).unwrap();
    let mut big = base.clone();
    for i in 0..15 {
        big = big.with_item(i, &"y".repeat(200 * 1024));
    }
    let (updates, commits) = r.write::<App>(Some(&base), &Notes { hint: None, ..big }).unwrap();
    assert_eq!((updates.len(), commits), (1, 1));
    let frames = split(&updates[0], MAX_UPDATE_BYTES).unwrap();
    assert!(frames.len() >= 3, "{} fragments for {} bytes", frames.len(), updates[0].len());
    assert!(frames.iter().all(|f| f.len() <= MAX_UPDATE_BYTES));
    let mut r = craft_cloud_kit::raw::Replica::default();
    r.write::<App>(None, &base).unwrap();
    let (updates, _) = r.write::<App>(Some(&base), &big_rows(&base)).unwrap();
    assert!(updates[0].len() > MAX_UPDATE_BYTES, "the tests' 1 MB change is over a frame: {} bytes", updates[0].len());
    // A big paste of text: a bundle of slices of at most SLICE_OPS operations (Loro imports each in milliseconds).
    let mut r = craft_cloud_kit::raw::Replica::default();
    let (none, _) = r.write::<TestApp>(None, &Doc::empty()).unwrap();
    let mut pasted = Doc::empty();
    pasted.story = Arc::new(RichText::plain(&"t".repeat(300_000)));
    let (updates, _) = r.write::<TestApp>(Some(&Doc::empty()), &pasted).unwrap();
    assert!(none.len() <= 1 && updates.len() == 1);
    let slices = craft_cloud_api::fragments::parts(&updates[0]).unwrap();
    assert!(slices.len() >= 300_000 / craft_cloud_api::fragments::SLICE_OPS as usize, "{} slices", slices.len());
    for s in slices {
        let meta = loro::LoroDoc::decode_import_blob_meta(s, false).unwrap();
        let ops: i32 = meta.partial_end_vv.iter().map(|(p, e)| e - meta.partial_start_vv.get(p).copied().unwrap_or(0)).sum();
        assert!(ops <= craft_cloud_api::fragments::SLICE_OPS, "{ops} operations in a slice");
    }
}

/// A 5 MB change (twenty 250 KiB notes) goes as one update in fragments and arrives whole: the other machine's model
/// has none or all of it at every poll. Its undo, as big, goes and arrives whole too.
#[test]
fn a_five_megabyte_change_arrives_whole_and_undoes_whole() {
    setup();
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "big.txt", Notes::new("big", &[]));
    let mut b = open::<App>(&laptop, "big.txt", Notes::default());
    let before = hub.updates("big.txt");
    let notes: Vec<String> = (0..20).map(|i| format!("{i:02}{}", "x".repeat(250 * 1024))).collect();
    change(&mut a, |m| {
        let mut items = (*m.items).clone();
        let mut hint = Vec::new();
        for n in &notes {
            let id = ElemId::new();
            hint.push(id);
            items.push((id, n.clone()));
        }
        Notes { items: Arc::new(items), hint: Some(hint), ..m.clone() }
    })
    .unwrap();
    until(&mut [&mut a, &mut b], "the laptop has all 5 MB", |s| {
        let n = s[1].model().items.len();
        assert!(n == 0 || n == 20, "the laptop held {n} of the 20 notes");
        n == 20
    });
    assert_eq!(b.model().texts(), notes);
    let sent = hub.updates("big.txt") - before;
    assert!(sent >= 70, "{sent} fragments of 64 KiB for one 5 MB update");
    assert!(a.status().error.is_none() && b.status().error.is_none(), "{:?} {:?}", a.status().error, b.status().error);
    // One undo step takes the whole change back, on both, whole.
    assert!(a.undo().unwrap());
    until(&mut [&mut a, &mut b], "the undo is on both", |s| {
        let n = s[1].model().items.len();
        assert!(n == 0 || n == 20, "the laptop held {n} of the 20 notes during the undo");
        s.iter().all(|s| s.model().items.is_empty())
    });
}

/// The other machine gets a 1 MB change part of the way (its first fragment), and meanwhile adds a row, edits and
/// undoes a cell, and changes the title: its model never shows part of the change, nothing of it is written back,
/// and once the change comes whole (the set stopped coming: a gap, and the catch-up brings it), every row of it and
/// every local edit are on both machines.
#[test]
fn local_edits_while_a_change_comes_in_fragments_keep_both() {
    setup();
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "rows.txt", Notes::new("t", &["first"]));
    let mut b = open::<App>(&laptop, "rows.txt", Notes::default());
    until(&mut [&mut a, &mut b], "the laptop has the document", |s| s[1].model().texts() == ["first"]);
    change(&mut b, |m| m.with_item(1, "local")).unwrap();
    until(&mut [&mut a, &mut b], "the pc has the laptop's row", |s| s[0].model().items.len() == 2);
    laptop.set_fragment_faults(Some(FIRST_ONLY));
    change(&mut a, big_rows).unwrap();
    let small = |s: &Session<App>| s.model().items.iter().all(|(_, t)| t.len() < 1000);
    poll_for(&mut [&mut a, &mut b], Duration::from_millis(200), |s| small(&*s[1]));
    // Meanwhile on the laptop: a cell edited and undone, and the title.
    change(&mut b, |m| m.with_edit(0, "first, edited")).unwrap();
    assert!(b.undo().unwrap());
    change(&mut b, |m| m.with_title("the laptop's")).unwrap();
    assert!(small(&b));
    laptop.set_fragment_faults(None);
    until(&mut [&mut a, &mut b], "both have everything", |s| {
        s.iter().all(|s| s.model().items.len() == 8 && *s.model().title == "the laptop's" && s.model().texts().contains(&"first".to_owned()))
    });
    assert!(b.model().texts().contains(&"local".to_owned()));
    assert_eq!(a.model(), b.model());
    assert!(b.status().gaps >= 1, "the set stopped coming: the laptop caught up");
}

/// The same with the change in a tree node's story (a 300 KB paste into a layer's text): meanwhile the other machine
/// renames that node and types at the start of its story. Both keep everything.
#[test]
fn edits_of_the_same_node_and_story_while_a_paste_comes_keep_both() {
    setup();
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let id = ElemId::new();
    let mut start = Doc::empty();
    let mut node = Node::leaf(id, "layer");
    node.text = RichText::plain("abc");
    start.layers = Arc::new(vec![Arc::new(node)]);
    let mut a = open::<TestApp>(&pc, "paste.kitdemo", start);
    let mut b = open::<TestApp>(&laptop, "paste.kitdemo", Doc::empty());
    until(&mut [&mut a, &mut b], "the laptop has the layer", |s| s[1].model().layers.len() == 1);
    laptop.set_fragment_faults(Some(FIRST_ONLY));
    // (300 KB: in fragments, as the test's frames are 64 KiB. Loro's import of a big paste into a text this machine
    // edits at the same moment takes time growing with the square of its size, seconds for a megabyte.)
    let paste = "p".repeat(300 * 1024);
    change(&mut a, |d| Doc {
        layers: Arc::new(model::update(&d.layers, id, &|n| Node { text: RichText::plain(&format!("abc{paste}")), ..n.clone() })),
        ..d.clone()
    })
    .unwrap();
    let short = |s: &Session<TestApp>| s.model().layers.iter().all(|n| n.text.text.len() < 1000);
    poll_for(&mut [&mut a, &mut b], Duration::from_millis(200), |s| short(&*s[1]));
    change(&mut b, |d| {
        let renamed = model::rename(&d.layers, id, "renamed");
        Doc { layers: Arc::new(model::update(&renamed, id, &|n| Node { text: model::type_at(&n.text, 0, "hello "), ..n.clone() })), ..d.clone() }
    })
    .unwrap();
    assert!(short(&b));
    laptop.set_fragment_faults(None);
    let want = format!("hello abc{paste}");
    until(&mut [&mut a, &mut b], "both have the paste, the name and the typing", |s| {
        s.iter().all(|s| s.model().layers.first().is_some_and(|n| n.name == "renamed" && n.text.text == want))
    });
}

/// A save while a change comes in fragments (its first one in): the record names only what the model holds.
#[test]
fn a_save_while_a_change_comes_records_what_the_model_holds() {
    setup();
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "save.txt", Notes::new("t", &["first"]));
    let mut b = open::<App>(&laptop, "save.txt", Notes::default());
    until(&mut [&mut a, &mut b], "the laptop has the document", |s| s[1].model().texts() == ["first"]);
    let before = b.save_point().unwrap().frontiers().to_vec();
    laptop.set_fragment_faults(Some(FIRST_ONLY));
    change(&mut a, big_rows).unwrap();
    poll_for(&mut [&mut a, &mut b], Duration::from_millis(200), |s| s[1].model().items.len() == 1);
    let during = b.save_point().unwrap();
    assert_eq!(during.frontiers(), before.as_slice(), "the record of a save made now names nothing of the change");
    assert_eq!(during.model().items.len(), 1);
    assert!(!b.file_behind());
}

/// The pc's agent stops after the first fragment of a 1 MB change (it crashed part of the way): no machine gets any
/// of it, and nothing waits for it, so a small change from a third machine reaches the laptop at once. Once the pc's
/// agent is back, the pc's session sends the change again, and it comes whole.
#[test]
fn a_sender_stopped_part_of_the_way_holds_nothing_up_and_sends_again() {
    setup();
    let hub = Hub::new();
    let (pc, laptop, tablet) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap(), hub.machine("tablet").unwrap());
    let mut a = open::<App>(&pc, "gone.txt", Notes::new("t", &["first"]));
    let mut b = open::<App>(&laptop, "gone.txt", Notes::default());
    let mut c = open::<App>(&tablet, "gone.txt", Notes::default());
    until(&mut [&mut a, &mut b, &mut c], "all have the document", |s| s.iter().all(|s| s.model().texts() == ["first"]));
    pc.drop_connections_after(1);
    change(&mut a, big_rows).unwrap();
    pc.restart();
    poll_for(&mut [&mut b, &mut c], Duration::from_millis(300), |s| s.iter().all(|s| s.model().items.len() == 1));
    let sent = Instant::now();
    change(&mut c, |m| m.with_title("from the tablet")).unwrap();
    until(&mut [&mut b, &mut c], "the tablet's change on the laptop", |s| *s[0].model().title == "from the tablet");
    assert!(sent.elapsed() < Duration::from_secs(2), "a later change waited {:?}", sent.elapsed());
    assert_eq!(b.model().items.len(), 1, "nothing of the pc's change");
    pc.start();
    a.reconnect_now();
    until(&mut [&mut a, &mut b, &mut c], "the pc's change whole on every machine", |s| {
        s.iter().all(|s| s.model().items.len() == 7 && *s.model().title == "from the tablet")
    });
}

/// The laptop's agent restarts with only the first fragment of a 1 MB change on its way to the app: the session
/// opens the document again, and its state brings the change whole.
#[test]
fn a_receiver_restarted_part_of_the_way_gets_the_change_whole() {
    setup();
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "back.txt", Notes::new("t", &["first"]));
    let mut b = open::<App>(&laptop, "back.txt", Notes::default());
    until(&mut [&mut a, &mut b], "the laptop has the document", |s| s[1].model().texts() == ["first"]);
    laptop.set_fragment_faults(Some(FIRST_ONLY));
    change(&mut a, big_rows).unwrap();
    poll_for(&mut [&mut a, &mut b], Duration::from_millis(100), |s| s[1].model().items.len() == 1);
    laptop.restart();
    laptop.set_fragment_faults(None);
    laptop.start();
    b.reconnect_now();
    until(&mut [&mut a, &mut b], "the laptop has the change whole", |s| {
        let n = s[1].model().items.len();
        assert!(n == 1 || n == 7, "the laptop held part of the change");
        n == 7
    });
    assert_eq!(a.model(), b.model());
}
