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
    let layers =
        vec![Arc::new(Node { id: x, name: "x".into(), children: Vec::new() }), Arc::new(Node { id: y, name: "y".into(), children: Vec::new() })];
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
