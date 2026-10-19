//! The mappings, without a session: every shape round-trips (model, Loro, model), merges (two replicas with random
//! edits end equal, and normalised to a model the app could make), and writes nothing for what didn't change. Then the
//! cases each shape has to get right: marks across a split and a join, typing at one spot at once, rows inserted at
//! once, moves that would make a cycle, keyframes added at once.

mod model;

use std::sync::Arc;

use craft_cloud_kit::raw::Replica;
use craft_cloud_kit::{RichText, Value};
use model::{Doc, Grid, Item, Key, Node, Rng, SHAPES, Shape, TestApp, Timeline, edit, valid};
use proptest::prelude::*;

const TREES: &[&str] = &["layers"];

/// Reads a replica's whole model and normalises it as a session does (writing back what that repaired).
fn settle(r: &mut Replica) -> Doc {
    let base = Arc::new(Doc::empty());
    let raw = r.read::<TestApp>(&base).unwrap();
    let mut fixed = (*raw).clone();
    <TestApp as craft_cloud_kit::LiveDocument>::normalise(&mut fixed);
    r.write::<TestApp>(Some(&raw), &fixed).unwrap();
    <TestApp as craft_cloud_kit::LiveDocument>::refresh_derived(&mut fixed);
    fixed
}

fn shape_doc(rng: &mut Rng, shape: Shape, n: usize) -> Doc {
    let mut d = Doc::empty();
    for _ in 0..n {
        d = edit(&d, shape, rng);
    }
    d
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, .. ProptestConfig::default() })]

    /// Model to Loro and back: an import, then every change after it, read back exactly.
    #[test]
    fn every_shape_round_trips(seed in any::<u64>(), shape in 0usize..6) {
        let mut rng = Rng(seed);
        let mut doc = match SHAPES.get(shape) {
            Some(s) => shape_doc(&mut rng, *s, 12),
            None => model::random_doc(&mut rng, 30),
        };
        let mut r = Replica::default();
        r.write::<TestApp>(None, &doc).unwrap();
        r.reindex(TREES);
        prop_assert_eq!(&settle(&mut r), &doc);
        for _ in 0..12 {
            let s = SHAPES.get(shape).copied().unwrap_or(SHAPES[rng.below(5)]);
            let next = edit(&doc, s, &mut rng);
            r.write::<TestApp>(Some(&doc), &next).unwrap();
            doc = settle(&mut r);
            prop_assert_eq!(&doc, &next);
        }
    }

    /// Two replicas from one import, random edits on both, then each takes the other's: both end equal, and valid.
    #[test]
    fn every_shape_merges(seed in any::<u64>(), shape in 0usize..6) {
        let mut rng = Rng(seed);
        let start = model::random_doc(&mut rng, 15);
        let mut a = Replica::default();
        let (import, _) = a.write::<TestApp>(None, &start).unwrap();
        a.reindex(TREES);
        let mut b = Replica::default();
        b.import(&import, TREES).unwrap();
        let (mut da, mut db) = (settle(&mut a), settle(&mut b));
        for _round in 0..3 {
            let (mut out_a, mut out_b) = (Vec::new(), Vec::new());
            for _ in 0..rng.below(6) {
                let s = SHAPES.get(shape).copied().unwrap_or(SHAPES[rng.below(5)]);
                let next = edit(&da, s, &mut rng);
                out_a.extend(a.write::<TestApp>(Some(&da), &next).unwrap().0);
                da = next;
            }
            for _ in 0..rng.below(6) {
                let s = SHAPES.get(shape).copied().unwrap_or(SHAPES[rng.below(5)]);
                let next = edit(&db, s, &mut rng);
                out_b.extend(b.write::<TestApp>(Some(&db), &next).unwrap().0);
                db = next;
            }
            a.import(&out_b, TREES).unwrap();
            b.import(&out_a, TREES).unwrap();
            settle(&mut a);
            settle(&mut b);
            // What normalising wrote on each goes to the other too.
            let (fa, fb) = (a.doc.export(loro_updates(&b)).unwrap(), b.doc.export(loro_updates(&a)).unwrap());
            b.import(&[fa], TREES).unwrap();
            a.import(&[fb], TREES).unwrap();
            da = settle(&mut a);
            db = settle(&mut b);
            prop_assert_eq!(&da, &db);
            prop_assert!(valid(&da).is_ok(), "{:?}", valid(&da));
        }
    }
}

fn loro_updates(other: &Replica) -> loro::ExportMode<'static> {
    loro::ExportMode::updates_owned(other.doc.oplog_vv())
}

/// Ops one change made.
fn ops(r: &mut Replica, old: &Doc, new: &Doc) -> i64 {
    let before = r.ops();
    r.write::<TestApp>(Some(old), new).unwrap();
    r.ops() - before
}

/// Unchanged parts write nothing: the same content in new allocations writes nothing, and one small change in one
/// part writes one operation (unchanged subtrees, cells and items aren't even compared).
#[test]
fn unchanged_parts_write_nothing() {
    let mut rng = Rng(7);
    let mut r = Replica::default();
    let doc = model::random_doc(&mut rng, 60);
    let doc = {
        let mut d = doc;
        if model::flatten(&d.layers).is_empty() {
            d.layers = Arc::new(vec![Arc::new(Node { id: rng.id(), name: "l".into(), children: Vec::new() })]);
        }
        d
    };
    r.write::<TestApp>(None, &doc).unwrap();
    r.reindex(TREES);
    let doc = settle(&mut r);
    // Equal content, every part in a new allocation: compared, nothing written.
    let copy = Doc {
        layers: Arc::new(doc.layers.iter().map(|n| Arc::new((**n).clone())).collect()),
        story: Arc::new((*doc.story).clone()),
        grid: Arc::new((*doc.grid).clone()),
        timeline: Arc::new((*doc.timeline).clone()),
        settings: Arc::new((*doc.settings).clone()),
        words: doc.words,
    };
    assert_eq!(ops(&mut r, &doc, &copy), 0, "the same content writes nothing");
    let doc = copy;
    // One keystroke.
    let typed = Doc { story: Arc::new(model::type_at(&doc.story, 0, "Z")), ..doc.clone() };
    assert_eq!(ops(&mut r, &doc, &typed), 1, "a keystroke is one insert");
    let doc = typed;
    // One layer renamed.
    let first = model::flatten(&doc.layers)[0].0;
    let renamed = Doc { layers: Arc::new(model::rename(&doc.layers, first, "renamed")), ..doc.clone() };
    assert_eq!(ops(&mut r, &doc, &renamed), 1, "one property");
    let doc = renamed;
    // One cell, one keyframe value, the settings.
    let mut g = (*doc.grid).clone();
    if g.rows.is_empty() || g.cols.is_empty() {
        g.rows.push(rng.id());
        g.cols.push(rng.id());
        let next = Doc { grid: Arc::new(g.clone()), ..doc.clone() };
        r.write::<TestApp>(Some(&doc), &next).unwrap();
    }
    let doc = Doc { grid: Arc::new(g.clone()), ..doc };
    g.cells.insert((g.rows[0], g.cols[0]), "cell!".into());
    let celled = Doc { grid: Arc::new(g), ..doc.clone() };
    assert_eq!(ops(&mut r, &doc, &celled), 1, "one cell");
    let doc = celled;
    let settings = Doc { settings: Arc::new(vec![1, 2, 3]), ..doc.clone() };
    assert_eq!(ops(&mut r, &doc, &settings), 1, "one value");
}

/// Two replicas of `doc`.
fn pair(doc: &Doc) -> (Replica, Replica, Doc) {
    let mut a = Replica::default();
    let (import, _) = a.write::<TestApp>(None, doc).unwrap();
    a.reindex(TREES);
    let mut b = Replica::default();
    b.import(&import, TREES).unwrap();
    let d = settle(&mut a);
    (a, b, d)
}

/// Each takes the other's changes; both models, equal.
fn exchange(a: &mut Replica, b: &mut Replica) -> Doc {
    let (fa, fb) = (a.doc.export(loro_updates(b)).unwrap(), b.doc.export(loro_updates(a)).unwrap());
    b.import(&[fa], TREES).unwrap();
    a.import(&[fb], TREES).unwrap();
    let (da, db) = (settle(a), settle(b));
    assert_eq!(da, db);
    assert!(valid(&da).is_ok(), "{:?}", valid(&da));
    da
}

fn with_story(text: RichText) -> Doc {
    Doc { story: Arc::new(text), ..Doc::empty() }
}

/// Character marks across a paragraph split and a join, made on one machine while the other formats the same words:
/// both end with the bold across both paragraphs, then across the joined one; the split paragraph keeps its heading
/// on both halves.
#[test]
fn marks_survive_a_split_and_a_join() {
    let base = model::format_para(&RichText::plain("Hello world"), 0, "h", Some(Value::I64(1)));
    let (mut a, mut b, doc) = pair(&with_story(base));
    // A splits the paragraph after "Hello"; B makes "lo wo" bold.
    let split = Doc { story: Arc::new(model::type_at(&doc.story, 5, "\n")), ..doc.clone() };
    a.write::<TestApp>(Some(&doc), &split).unwrap();
    let bold = Doc { story: Arc::new(model::format(&doc.story, 3..8, "b", Some(Value::Bool(true)))), ..doc.clone() };
    b.write::<TestApp>(Some(&doc), &bold).unwrap();
    let merged = exchange(&mut a, &mut b);
    assert_eq!(merged.story.text, "Hello\n world");
    let bolded: String = merged.story.text.char_indices().filter(|(i, _)| merged.story.attrs_at(*i).contains_key("b")).map(|(_, c)| c).collect();
    assert_eq!(bolded.replace('\n', ""), "lo wo", "the bold is on both sides of the split");
    assert_eq!(merged.story.paragraphs.len(), 2);
    assert!(merged.story.paragraphs.iter().all(|p| p.get("h") == Some(&Value::I64(1))), "{:?}", merged.story.paragraphs);
    // A joins them again; B italicises "world".
    let joined = Doc { story: Arc::new(model::delete(&merged.story, 5..6)), ..merged.clone() };
    a.write::<TestApp>(Some(&merged), &joined).unwrap();
    let italic = Doc { story: Arc::new(model::format(&merged.story, 7..12, "i", Some(Value::Bool(true)))), ..merged.clone() };
    b.write::<TestApp>(Some(&merged), &italic).unwrap();
    let merged = exchange(&mut a, &mut b);
    assert_eq!(merged.story.text, "Hello world");
    assert_eq!(merged.story.paragraphs.len(), 1);
    let bolded: String = merged.story.text.char_indices().filter(|(i, _)| merged.story.attrs_at(*i).contains_key("b")).map(|(_, c)| c).collect();
    assert_eq!(bolded, "lo wo");
    let italic: String = merged.story.text.char_indices().filter(|(i, _)| merged.story.attrs_at(*i).contains_key("i")).map(|(_, c)| c).collect();
    assert_eq!(italic, "world");
}

/// Two machines typing at the same spot at once: both words, each whole (not interleaved).
#[test]
fn typing_at_one_spot_at_once_keeps_both_words() {
    let (mut a, mut b, doc) = pair(&with_story(RichText::plain("ab")));
    let ta = Doc { story: Arc::new(model::type_at(&doc.story, 1, "XYZ")), ..doc.clone() };
    a.write::<TestApp>(Some(&doc), &ta).unwrap();
    let tb = Doc { story: Arc::new(model::type_at(&doc.story, 1, "123")), ..doc.clone() };
    b.write::<TestApp>(Some(&doc), &tb).unwrap();
    let merged = exchange(&mut a, &mut b);
    assert!(merged.story.text == "aXYZ123b" || merged.story.text == "a123XYZb", "{}", merged.story.text);
}

/// Two machines inserting a row at the same place at once: both rows, each with its cells.
#[test]
fn rows_inserted_at_once_are_both_kept() {
    let mut rng = Rng(3);
    let (r0, c0) = (rng.id(), rng.id());
    let grid = Grid { rows: vec![r0], cols: vec![c0], cells: [((r0, c0), "first".to_owned())].into() };
    let (mut a, mut b, doc) = pair(&Doc { grid: Arc::new(grid), ..Doc::empty() });
    let (ra, rb) = (rng.id(), rng.id());
    let mut ga = (*doc.grid).clone();
    ga.rows.insert(0, ra);
    ga.cells.insert((ra, c0), "from a".into());
    let mut gb = (*doc.grid).clone();
    gb.rows.insert(0, rb);
    gb.cells.insert((rb, c0), "from b".into());
    a.write::<TestApp>(Some(&doc), &Doc { grid: Arc::new(ga), ..doc.clone() }).unwrap();
    b.write::<TestApp>(Some(&doc), &Doc { grid: Arc::new(gb), ..doc.clone() }).unwrap();
    let merged = exchange(&mut a, &mut b);
    assert_eq!(merged.grid.rows.len(), 3);
    assert_eq!(merged.grid.rows.last(), Some(&r0));
    assert_eq!(merged.grid.cells.get(&(ra, c0)).map(String::as_str), Some("from a"));
    assert_eq!(merged.grid.cells.get(&(rb, c0)).map(String::as_str), Some("from b"));
    // A row deleted on one machine while its cell is edited on the other: the cell isn't read back.
    let mut gd = (*merged.grid).clone();
    gd.rows.retain(|r| *r != ra);
    gd.cells.retain(|(r, _), _| *r != ra);
    let mut ge = (*merged.grid).clone();
    ge.cells.insert((ra, c0), "edited".into());
    a.write::<TestApp>(Some(&merged), &Doc { grid: Arc::new(gd), ..merged.clone() }).unwrap();
    b.write::<TestApp>(Some(&merged), &Doc { grid: Arc::new(ge), ..merged.clone() }).unwrap();
    let merged = exchange(&mut a, &mut b);
    assert!(!merged.grid.rows.contains(&ra) && !merged.grid.cells.contains_key(&(ra, c0)));
}

/// Two machines moving two layers under each other at once: Loro drops one move, both end with the same tree and no
/// cycle, and both layers are kept.
#[test]
fn moves_that_would_make_a_cycle_end_in_one_tree() {
    let mut rng = Rng(11);
    let (x, y) = (rng.id(), rng.id());
    let layers =
        vec![Arc::new(Node { id: x, name: "x".into(), children: Vec::new() }), Arc::new(Node { id: y, name: "y".into(), children: Vec::new() })];
    let (mut a, mut b, doc) = pair(&Doc { layers: Arc::new(layers), ..Doc::empty() });
    let (rest, taken) = model::remove(&doc.layers, x);
    let a_moves = Doc { layers: Arc::new(model::insert(&rest, Some(y), 0, taken.unwrap())), ..doc.clone() };
    let (rest, taken) = model::remove(&doc.layers, y);
    let b_moves = Doc { layers: Arc::new(model::insert(&rest, Some(x), 0, taken.unwrap())), ..doc.clone() };
    a.write::<TestApp>(Some(&doc), &a_moves).unwrap();
    b.write::<TestApp>(Some(&doc), &b_moves).unwrap();
    let merged = exchange(&mut a, &mut b);
    let all = model::flatten(&merged.layers);
    assert_eq!(all.len(), 2, "both layers kept: {all:?}");
    assert_eq!(merged.layers.len(), 1, "one under the other, no cycle");
}

/// Two machines adding keyframes to one item at once: both kept; and an item trimmed on one machine while moved to
/// another track on the other keeps both changes.
#[test]
fn keyframes_added_at_once_on_one_item_are_both_kept() {
    let mut rng = Rng(5);
    let (t1, t2, item) = (rng.id(), rng.id(), rng.id());
    let tl = Timeline {
        tracks: vec![(t1, "video".into()), (t2, "audio".into())],
        items: [(item, Item { track: t1, start: 10, len: 50, name: "clip".into() })].into(),
        keys: Default::default(),
    };
    let (mut a, mut b, doc) = pair(&Doc { timeline: Arc::new(tl), ..Doc::empty() });
    let (ka, kb) = (rng.id(), rng.id());
    let mut ta = (*doc.timeline).clone();
    ta.keys.insert(ka, Key { item, at: 5, value: 1 });
    let mut tb = (*doc.timeline).clone();
    tb.keys.insert(kb, Key { item, at: 5, value: 2 });
    a.write::<TestApp>(Some(&doc), &Doc { timeline: Arc::new(ta), ..doc.clone() }).unwrap();
    b.write::<TestApp>(Some(&doc), &Doc { timeline: Arc::new(tb), ..doc.clone() }).unwrap();
    let merged = exchange(&mut a, &mut b);
    assert_eq!(merged.timeline.keys.len(), 2);
    let mut ta = (*merged.timeline).clone();
    ta.items.get_mut(&item).unwrap().len = 20;
    let mut tb = (*merged.timeline).clone();
    tb.items.get_mut(&item).unwrap().track = t2;
    a.write::<TestApp>(Some(&merged), &Doc { timeline: Arc::new(ta), ..merged.clone() }).unwrap();
    b.write::<TestApp>(Some(&merged), &Doc { timeline: Arc::new(tb), ..merged.clone() }).unwrap();
    let merged = exchange(&mut a, &mut b);
    let i = &merged.timeline.items[&item];
    assert_eq!((i.track, i.len), (t2, 20), "both changes kept");
    assert_eq!(merged.timeline.keys.len(), 2, "its keyframes go with it");
}
