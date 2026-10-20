//! Undo while live, two sessions on the fake agent: undo takes back only this machine's change (the other's edit to
//! the same element stays), redo puts it back, a group is one step, and a change split into several commits is one
//! step.

mod common;
mod model;

use std::sync::Arc;

use common::{change, open, until};
use craft_cloud_app::fake::Hub;
use craft_cloud_kit::{RichText, Session, UndoKind};
use model::{Doc, Node, TestApp};

fn story(s: &Session<TestApp>) -> String {
    s.model().story.text.clone()
}

fn type_end(doc: &Doc, s: &str) -> Doc {
    let len = doc.story.text.chars().count();
    let mut d = Doc { story: Arc::new(model::type_at(&doc.story, len, s)), ..doc.clone() };
    <TestApp as craft_cloud_kit::LiveDocument>::refresh_derived(&mut d);
    d
}

fn pair(hub: &Hub, start: Doc) -> (Session<TestApp>, Session<TestApp>, craft_cloud_app::fake::Fake, craft_cloud_app::fake::Fake) {
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let a = open::<TestApp>(&pc, "u.kitdemo", start);
    let b = open::<TestApp>(&laptop, "u.kitdemo", Doc::empty());
    (a, b, pc, laptop)
}

#[test]
fn undo_takes_back_only_this_machines_change_and_redo_puts_it_back() {
    let hub = Hub::new();
    let start = Doc { story: Arc::new(RichText::plain("Note: ")), ..Doc::empty() };
    let (mut a, mut b, _pc, _laptop) = pair(&hub, start);
    assert!(!a.can_undo(), "opening isn't a step");
    assert_eq!(a.undo_kind(), UndoKind::Live);
    change(&mut a, |d| type_end(d, "hello")).unwrap();
    until(&mut [&mut a, &mut b], "the laptop has it", |s| s.iter().all(|s| story(s) == "Note: hello"));
    // The other machine edits the same text right after.
    change(&mut b, |d| type_end(d, " world")).unwrap();
    until(&mut [&mut a, &mut b], "both have both", |s| s.iter().all(|s| story(s) == "Note: hello world"));
    assert!(a.undo().unwrap());
    assert_eq!(story(&a), "Note:  world", "only this machine's word went, at once");
    until(&mut [&mut a, &mut b], "the undo reached the laptop", |s| s.iter().all(|s| story(s) == "Note:  world"));
    assert!(a.can_redo());
    assert!(a.redo().unwrap());
    until(&mut [&mut a, &mut b], "the redo too", |s| s.iter().all(|s| story(s) == "Note: hello world"));
    // The laptop's own undo takes back its word only.
    assert!(b.undo().unwrap());
    until(&mut [&mut a, &mut b], "the laptop's undo", |s| s.iter().all(|s| story(s) == "Note: hello"));
    assert!(!b.undo().unwrap(), "nothing more of the laptop's");
}

/// The same element: a layer renamed here and moved there. Undo takes back the rename; the move stays.
#[test]
fn undo_after_a_remote_edit_to_the_same_element_keeps_the_remote_edit() {
    let hub = Hub::new();
    let (x, y) = (craft_cloud_kit::ElemId::new(), craft_cloud_kit::ElemId::new());
    let layers = vec![Arc::new(Node::leaf(x, "x")), Arc::new(Node::leaf(y, "y"))];
    let (mut a, mut b, _pc, _laptop) = pair(&hub, Doc { layers: Arc::new(layers), ..Doc::empty() });
    change(&mut a, |d| Doc { layers: Arc::new(model::rename(&d.layers, x, "renamed")), ..d.clone() }).unwrap();
    until(&mut [&mut a, &mut b], "renamed on both", |s| s.iter().all(|s| model::find(&s.model().layers, x).is_some_and(|n| n.name == "renamed")));
    change(&mut b, |d| {
        let (rest, taken) = model::remove(&d.layers, x);
        Doc { layers: Arc::new(model::insert(&rest, Some(y), 0, taken.unwrap())), ..d.clone() }
    })
    .unwrap();
    let under_y = |s: &Session<TestApp>| model::flatten(&s.model().layers).contains(&(x, Some(y)));
    until(&mut [&mut a, &mut b], "moved on both", |s| s.iter().all(|s| under_y(s)));
    assert!(a.undo().unwrap());
    until(&mut [&mut a, &mut b], "the rename undone, the move kept", |s| {
        s.iter().all(|s| under_y(s) && model::find(&s.model().layers, x).is_some_and(|n| n.name == "x"))
    });
}

/// Typing grouped into one step; a big change (here 1.25 MB of cells, split into several commits under the cap) is
/// one step too. (Loro's undo of one big run of text takes time growing with its square, about 10 s for 1 MB: the
/// report's concerns, so the big change here is cells.)
#[test]
fn a_group_and_a_split_change_are_one_step_each() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _laptop) = pair(&hub, Doc { story: Arc::new(RichText::plain("")), ..Doc::empty() });
    a.group(|s| {
        for word in ["one", " two", " three"] {
            change(s, |d| type_end(d, word)).unwrap();
        }
    });
    assert!(a.begin_group());
    change(&mut a, |d| type_end(d, " four")).unwrap();
    a.end_group();
    let col = craft_cloud_kit::ElemId::new();
    let big = |d: &Doc| {
        let mut g = model::Grid { cols: vec![col], ..Default::default() };
        for i in 0..5 {
            let row = craft_cloud_kit::ElemId::new();
            g.rows.push(row);
            g.cells.insert((row, col), format!("{i}{}", "c".repeat(250 * 1024)));
        }
        Doc { grid: Arc::new(g), ..d.clone() }
    };
    let sent = hub.updates("u.kitdemo");
    change(&mut a, big).unwrap();
    until(&mut [&mut a, &mut b], "the big change arrived", |s| s.iter().all(|s| s.model().grid.cells.len() == 5));
    assert!(hub.updates("u.kitdemo") - sent >= 3, "split into several updates");
    assert!(a.undo().unwrap(), "the big change");
    until(&mut [&mut a, &mut b], "the big change undone in one step", |s| {
        s.iter().all(|s| s.model().grid.rows.is_empty() && story(s) == "one two three four")
    });
    assert!(a.undo().unwrap(), "the second group");
    assert_eq!(story(&a), "one two three");
    assert!(a.undo().unwrap(), "the first group");
    assert_eq!(story(&a), "");
    until(&mut [&mut a, &mut b], "empty on both", |s| s.iter().all(|s| story(s).is_empty()));
    assert!(a.redo().unwrap() && a.redo().unwrap() && a.redo().unwrap());
    until(&mut [&mut a, &mut b], "all redone", |s| s.iter().all(|s| s.model().grid.cells.len() == 5 && story(s) == "one two three four"));
    assert!(a.status().error.is_none(), "{:?}", a.status().error);
}

/// An undo asked for during a gesture (remote changes held) changes nothing under the app: the gesture's change is
/// taken (its `old` is still the session's model), and the undo runs once the gesture ends, after the held remote
/// change (the reviewer's probe).
#[test]
fn an_undo_during_a_gesture_waits_for_its_end() {
    use common::{App, Notes};
    let hub = Hub::new();
    let (pc, laptop) = (hub.machine("pc").unwrap(), hub.machine("laptop").unwrap());
    let mut a = open::<App>(&pc, "h.txt", Notes::new("t", &["x"]));
    let mut b = open::<App>(&laptop, "h.txt", Notes::default());
    change(&mut a, |m| m.with_item(1, "mine")).unwrap();
    until(&mut [&mut a, &mut b], "both", |s| s.iter().all(|s| s.model().texts().len() == 2));
    a.hold_remote(true);
    let gesture_old = a.model().clone();
    change(&mut b, |m| m.with_title("remote")).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
    while std::time::Instant::now() < deadline {
        b.poll();
        a.poll();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!a.undo().unwrap(), "deferred");
    assert!(Arc::ptr_eq(a.model(), &gesture_old), "nothing changed under the gesture");
    let gesture_new = Arc::new(gesture_old.with_edit(0, "dragged"));
    a.local_change(&gesture_old, &gesture_new).unwrap();
    a.hold_remote(false);
    assert!(a.poll().changed);
    // The remote title came, and the deferred undo took back the gesture's change (the last step).
    assert_eq!(*a.model().title, "remote");
    assert_eq!(a.model().texts(), vec!["x", "mine"]);
    until(&mut [&mut a, &mut b], "the same on both", |s| s.iter().all(|s| *s.model().title == "remote" && s.model().texts() == ["x", "mine"]));
}

fn layer_text(s: &Session<TestApp>) -> String {
    s.model().layers.first().map(|n| n.text.text.clone()).unwrap_or_default()
}

fn type_in_layer(d: &Doc, at: usize, s: &str) -> Doc {
    let Some(id) = d.layers.first().map(|n| n.id) else { return d.clone() };
    Doc { layers: Arc::new(model::update(&d.layers, id, &|n| Node { text: model::type_at(&n.text, at, s), ..n.clone() })), ..d.clone() }
}

/// Typing in one layer's story on both machines at once (a shape's text box), keystroke by keystroke, then one undoes:
/// only its own typing goes, on both machines; the other's stays where it was typed.
#[test]
fn typing_in_one_nodes_story_on_both_then_one_undoes() {
    let hub = Hub::new();
    let start = Doc {
        layers: Arc::new(vec![Arc::new(Node { text: RichText::plain("[]"), ..Node::leaf(craft_cloud_kit::ElemId::new(), "box") })]),
        ..Doc::empty()
    };
    let (mut a, mut b, _pc, _laptop) = pair(&hub, start);
    until(&mut [&mut a, &mut b], "the laptop has the box", |s| s.iter().all(|s| layer_text(s) == "[]"));
    // The pc types "abc" inside the brackets, the laptop "XY" after them, each keystroke a step, interleaved.
    for (k, (pc, laptop)) in [("a", Some("X")), ("b", Some("Y")), ("c", None)].into_iter().enumerate() {
        change(&mut a, |d| type_in_layer(d, 1 + k, pc)).unwrap();
        if let Some(t) = laptop {
            let at = layer_text(&b).chars().count();
            change(&mut b, |d| type_in_layer(d, at, t)).unwrap();
        }
        a.poll();
        b.poll();
    }
    until(&mut [&mut a, &mut b], "both have both", |s| s.iter().all(|s| layer_text(s) == "[abc]XY"));
    for _ in 0..3 {
        assert!(a.undo().unwrap());
    }
    until(&mut [&mut a, &mut b], "the pc's typing went on both", |s| s.iter().all(|s| layer_text(s) == "[]XY"));
    assert!(b.undo().unwrap());
    until(&mut [&mut a, &mut b], "the laptop's last keystroke went", |s| s.iter().all(|s| layer_text(s) == "[]X"));
    for _ in 0..3 {
        assert!(a.redo().unwrap());
    }
    until(&mut [&mut a, &mut b], "the pc's typing back", |s| s.iter().all(|s| layer_text(s) == "[abc]X"));
}

/// A change the app makes by itself (`background_change`: an upload that finished) goes to the other machine but is
/// never an undo step: undo takes back the user's change before it and leaves it.
#[test]
fn a_background_change_is_sent_but_never_undone() {
    let hub = Hub::new();
    let (mut a, mut b, _pc, _laptop) = pair(&hub, Doc { story: Arc::new(RichText::plain("s")), ..Doc::empty() });
    change(&mut a, |d| type_end(d, "!")).unwrap();
    let old = a.model().clone();
    let new = Arc::new(Doc { settings: Arc::new(vec![7, 7]), ..(*old).clone() });
    a.background_change(&old, &new).unwrap();
    until(&mut [&mut a, &mut b], "the laptop has both", |s| s.iter().all(|s| story(s) == "s!" && *s.model().settings == vec![7, 7]));
    assert!(a.undo().unwrap());
    until(&mut [&mut a, &mut b], "the typing went, the background change stayed", |s| {
        s.iter().all(|s| story(s) == "s" && *s.model().settings == vec![7, 7])
    });
    assert!(!a.undo().unwrap(), "the background change isn't a step");
    // A stale `old` is refused, as for a local change.
    let stale = Arc::new(Doc::empty());
    assert_eq!(a.background_change(&stale, &new), Err(craft_cloud_kit::KitError::Stale));
}

/// The story with `paste` added at its end, as one change (plain text: no runs to move).
fn paste(s: &mut Session<TestApp>, paste: &str) {
    change(s, |d| {
        let mut n = Doc { story: Arc::new(RichText::plain(&format!("{}{paste}", d.story.text))), ..d.clone() };
        <TestApp as craft_cloud_kit::LiveDocument>::refresh_derived(&mut n);
        n
    })
    .unwrap();
}

/// A step that changed up to 64 KiB of text undoes and redoes quickly; one byte more, and the step is refused
/// (quickly, never a frozen app) and stays as a barrier: nothing before it can be undone. A 1 MB paste likewise.
#[test]
fn a_step_too_big_to_undo_is_refused_and_a_barrier() {
    use craft_cloud_kit::{KitError, MAX_UNDO_TEXT_BYTES};
    use std::time::{Duration, Instant};
    let hub = Hub::new();
    let pc = hub.machine("pc").unwrap();
    let mut a = open::<TestApp>(&pc, "big.kitdemo", Doc { story: Arc::new(RichText::plain("Start.")), ..Doc::empty() });
    paste(&mut a, " small");
    // At the limit: undone and redone, quickly.
    paste(&mut a, &"x".repeat(MAX_UNDO_TEXT_BYTES));
    assert!(a.can_undo() && a.undo_blocked().is_none());
    let t = Instant::now();
    assert!(a.undo().unwrap());
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(story(&a), "Start. small");
    let t = Instant::now();
    assert!(a.redo().unwrap());
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(story(&a).len(), "Start. small".len() + MAX_UNDO_TEXT_BYTES);
    // One byte over: refused at once, and a barrier.
    paste(&mut a, &"y".repeat(MAX_UNDO_TEXT_BYTES + 1));
    assert!(!a.can_undo());
    assert!(a.undo_blocked().unwrap_or_default().contains("too large to undo while others edit"));
    for _ in 0..2 {
        let t = Instant::now();
        assert_eq!(a.undo(), Err(KitError::UndoTooLarge { bytes: MAX_UNDO_TEXT_BYTES + 1 }));
        assert!(t.elapsed() < Duration::from_millis(200), "{:?}", t.elapsed());
    }
    assert_eq!(story(&a).len(), "Start. small".len() + 2 * MAX_UNDO_TEXT_BYTES + 1, "nothing undone");
    // 1 MB.
    paste(&mut a, &"z".repeat(1024 * 1024));
    let t = Instant::now();
    assert_eq!(a.undo(), Err(KitError::UndoTooLarge { bytes: 1024 * 1024 }));
    assert!(t.elapsed() < Duration::from_millis(200), "{:?}", t.elapsed());
}
