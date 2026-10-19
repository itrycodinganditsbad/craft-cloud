//! A model with every shape the kit maps, for the mapping tests and the kit simulation: a layer tree, a story (rich
//! text), a grid (table), a timeline and settings (opaque), and random edits of each the way an app makes them
//! (copy-on-write, unchanged parts shared by `Arc`).
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;

use craft_cloud_kit::{
    Attrs, ElemId, KitError, LiveDocument, Mappings, Opaque, Props, RemotePatch, RichText, Span, TableSource, TextSource, TimelineSource, TreeSource,
    Value,
};

/// A small deterministic random source (SplitMix64).
#[derive(Debug, Clone)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }

    pub fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    pub fn id(&mut self) -> ElemId {
        ElemId(u128::from(self.next()) << 64 | u128::from(self.next()))
    }

    pub fn word(&mut self) -> String {
        const WORDS: &[&str] = &["a", "bc", "déf", "ghij", " ", "x", "日本", "🙂", "yz", "kl m"];
        WORDS[self.below(WORDS.len())].to_owned()
    }
}

// The tree.

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: ElemId,
    pub name: String,
    pub children: Vec<Arc<Node>>,
}

pub type Roots = Arc<Vec<Arc<Node>>>;

pub struct LayersSrc<'a> {
    roots: &'a Roots,
    index: HashMap<ElemId, &'a Arc<Node>>,
}

impl<'a> LayersSrc<'a> {
    pub fn new(roots: &'a Roots) -> Self {
        let mut index = HashMap::new();
        let mut stack: Vec<&'a Arc<Node>> = roots.iter().collect();
        while let Some(n) = stack.pop() {
            index.insert(n.id, n);
            stack.extend(n.children.iter());
        }
        Self { roots, index }
    }
}

impl TreeSource for LayersSrc<'_> {
    fn key(&self) -> Option<usize> {
        Some(Arc::as_ptr(self.roots) as usize)
    }
    fn roots(&self) -> Vec<ElemId> {
        self.roots.iter().map(|n| n.id).collect()
    }
    fn children(&self, id: ElemId) -> Vec<ElemId> {
        self.index.get(&id).map(|n| n.children.iter().map(|c| c.id).collect()).unwrap_or_default()
    }
    fn subtree_key(&self, id: ElemId) -> Option<usize> {
        self.index.get(&id).map(|n| Arc::as_ptr(n) as usize)
    }
    fn write(&self, id: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        if let Some(n) = self.index.get(&id) {
            props.set("name", n.name.as_str())?;
        }
        Ok(())
    }
}

/// Every node: id, parent, depth-first.
pub fn flatten(roots: &[Arc<Node>]) -> Vec<(ElemId, Option<ElemId>)> {
    let mut out = Vec::new();
    let mut stack: Vec<(&Arc<Node>, Option<ElemId>)> = roots.iter().rev().map(|n| (n, None)).collect();
    while let Some((n, parent)) = stack.pop() {
        out.push((n.id, parent));
        stack.extend(n.children.iter().rev().map(|c| (c, Some(n.id))));
    }
    out
}

fn contains(node: &Node, id: ElemId) -> bool {
    node.id == id || node.children.iter().any(|c| contains(c, id))
}

/// `roots` without `id`'s subtree, which is returned.
pub fn remove(roots: &[Arc<Node>], id: ElemId) -> (Vec<Arc<Node>>, Option<Arc<Node>>) {
    let mut taken = None;
    let mut out = Vec::new();
    for n in roots {
        if n.id == id {
            taken = Some(n.clone());
        } else if taken.is_none() && contains(n, id) {
            let (children, t) = remove(&n.children, id);
            taken = t;
            out.push(Arc::new(Node { id: n.id, name: n.name.clone(), children }));
        } else {
            out.push(n.clone());
        }
    }
    (out, taken)
}

/// `roots` with `node` under `parent` (top level when `None`) at `at`.
pub fn insert(roots: &[Arc<Node>], parent: Option<ElemId>, at: usize, node: Arc<Node>) -> Vec<Arc<Node>> {
    match parent {
        None => {
            let mut out = roots.to_vec();
            out.insert(at.min(out.len()), node);
            out
        }
        Some(p) => roots
            .iter()
            .map(|n| {
                if n.id == p {
                    let mut children = n.children.clone();
                    children.insert(at.min(children.len()), node.clone());
                    Arc::new(Node { id: n.id, name: n.name.clone(), children })
                } else if contains(n, p) {
                    Arc::new(Node { id: n.id, name: n.name.clone(), children: insert(&n.children, parent, at, node.clone()) })
                } else {
                    n.clone()
                }
            })
            .collect(),
    }
}

pub fn rename(roots: &[Arc<Node>], id: ElemId, name: &str) -> Vec<Arc<Node>> {
    roots
        .iter()
        .map(|n| {
            if n.id == id {
                Arc::new(Node { id, name: name.to_owned(), children: n.children.clone() })
            } else if contains(n, id) {
                Arc::new(Node { id: n.id, name: n.name.clone(), children: rename(&n.children, id, name) })
            } else {
                n.clone()
            }
        })
        .collect()
}

pub fn find(roots: &[Arc<Node>], id: ElemId) -> Option<&Arc<Node>> {
    for n in roots {
        if n.id == id {
            return Some(n);
        }
        if let Some(found) = find(&n.children, id) {
            return Some(found);
        }
    }
    None
}

// The story, as characters with attributes (small texts only: tests).

pub struct Exploded {
    pub chars: Vec<char>,
    pub attrs: Vec<Attrs>,
    pub paras: Vec<Attrs>,
}

pub fn explode(t: &RichText) -> Exploded {
    let mut chars = Vec::new();
    let mut attrs = Vec::new();
    for (i, c) in t.text.char_indices() {
        chars.push(c);
        attrs.push(t.attrs_at(i));
    }
    let mut paras = t.paragraphs.clone();
    paras.resize(t.text.matches('\n').count() + 1, Attrs::new());
    Exploded { chars, attrs, paras }
}

pub fn implode(e: &Exploded) -> RichText {
    let mut text = String::new();
    let mut runs: Vec<(Range<usize>, Attrs)> = Vec::new();
    for (c, a) in e.chars.iter().zip(&e.attrs) {
        let start = text.len();
        text.push(*c);
        let end = text.len();
        if a.is_empty() {
            continue;
        }
        match runs.last_mut() {
            Some((r, last)) if r.end == start && last == a => r.end = end,
            _ => runs.push((start..end, a.clone())),
        }
    }
    RichText { text, runs, paragraphs: e.paras.clone() }
}

/// The canonical form of a story (runs merged, none empty), as the app keeps it.
pub fn canonical(t: &RichText) -> RichText {
    implode(&explode(t))
}

pub struct StorySrc<'a>(pub &'a Arc<RichText>);

impl TextSource for StorySrc<'_> {
    fn key(&self) -> Option<usize> {
        Some(Arc::as_ptr(self.0) as usize)
    }
    fn text(&self) -> &str {
        &self.0.text
    }
    fn runs(&self) -> Vec<(Range<usize>, Attrs)> {
        self.0.runs.clone()
    }
    fn paragraphs(&self) -> Vec<Attrs> {
        self.0.paragraphs.clone()
    }
}

/// Inserts `s` at character `at`, as typing does: it takes the attributes of the character before, and new
/// paragraphs take the attributes of the one they split.
pub fn type_at(t: &RichText, at: usize, s: &str) -> RichText {
    let mut e = explode(t);
    let at = at.min(e.chars.len());
    let inherited = if at > 0 { e.attrs.get(at - 1).cloned().unwrap_or_default() } else { Attrs::new() };
    let para = e.chars.iter().take(at).filter(|c| **c == '\n').count();
    let split = s.matches('\n').count();
    let attrs = e.paras.get(para).cloned().unwrap_or_default();
    for _ in 0..split {
        e.paras.insert(para, attrs.clone());
    }
    for (k, c) in s.chars().enumerate() {
        e.chars.insert(at + k, c);
        e.attrs.insert(at + k, inherited.clone());
    }
    implode(&e)
}

/// Deletes characters `range`: the paragraphs it joins keep the first one's attributes.
pub fn delete(t: &RichText, range: Range<usize>) -> RichText {
    let mut e = explode(t);
    let end = range.end.min(e.chars.len());
    let start = range.start.min(end);
    let para = e.chars.iter().take(start).filter(|c| **c == '\n').count();
    let joined = e.chars.get(start..end).map_or(0, |cs| cs.iter().filter(|c| **c == '\n').count());
    for _ in 0..joined {
        if para + 1 < e.paras.len() {
            e.paras.remove(para + 1);
        }
    }
    e.chars.drain(start..end);
    e.attrs.drain(start..end);
    implode(&e)
}

/// Sets (or, with `None`, removes) a character attribute on characters `range`.
pub fn format(t: &RichText, range: Range<usize>, key: &str, value: Option<Value>) -> RichText {
    let mut e = explode(t);
    let end = range.end.min(e.chars.len());
    for a in e.attrs.iter_mut().take(end).skip(range.start) {
        match &value {
            Some(v) => a.insert(key.to_owned(), v.clone()),
            None => a.remove(key),
        };
    }
    implode(&e)
}

/// Sets (or removes) a paragraph attribute.
pub fn format_para(t: &RichText, para: usize, key: &str, value: Option<Value>) -> RichText {
    let mut e = explode(t);
    if let Some(p) = e.paras.get_mut(para) {
        match value {
            Some(v) => p.insert(key.to_owned(), v),
            None => p.remove(key),
        };
    }
    implode(&e)
}

// The grid.

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Grid {
    pub rows: Vec<ElemId>,
    pub cols: Vec<ElemId>,
    pub cells: BTreeMap<(ElemId, ElemId), String>,
}

pub struct GridSrc<'a>(pub &'a Arc<Grid>);

impl TableSource for GridSrc<'_> {
    fn key(&self) -> Option<usize> {
        Some(Arc::as_ptr(self.0) as usize)
    }
    fn rows(&self) -> Vec<ElemId> {
        self.0.rows.clone()
    }
    fn columns(&self) -> Vec<ElemId> {
        self.0.cols.clone()
    }
    fn cell(&self, row: ElemId, column: ElemId) -> Option<Value> {
        self.0.cells.get(&(row, column)).map(|s| Value::from(s.as_str()))
    }
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        self.0.cells.iter().map(|((r, c), s)| (*r, *c, Value::from(s.as_str()))).collect()
    }
}

// The timeline.

#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub track: ElemId,
    pub start: i64,
    pub len: i64,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub item: ElemId,
    pub at: i64,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Timeline {
    pub tracks: Vec<(ElemId, String)>,
    pub items: BTreeMap<ElemId, Item>,
    pub keys: BTreeMap<ElemId, Key>,
}

pub struct TimelineSrc<'a>(pub &'a Arc<Timeline>);

impl TimelineSource for TimelineSrc<'_> {
    fn key(&self) -> Option<usize> {
        Some(Arc::as_ptr(self.0) as usize)
    }
    fn tracks(&self) -> Vec<ElemId> {
        self.0.tracks.iter().map(|(id, _)| *id).collect()
    }
    fn write_track(&self, track: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        if let Some((_, name)) = self.0.tracks.iter().find(|(id, _)| *id == track) {
            props.set("name", name.as_str())?;
        }
        Ok(())
    }
    fn items(&self, track: ElemId) -> Vec<ElemId> {
        self.0.items.iter().filter(|(_, i)| i.track == track).map(|(id, _)| *id).collect()
    }
    fn span(&self, item: ElemId) -> Option<Span> {
        self.0.items.get(&item).map(|i| Span { start: i.start, len: i.len })
    }
    fn write_item(&self, item: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        if let Some(i) = self.0.items.get(&item) {
            props.set("name", i.name.as_str())?;
        }
        Ok(())
    }
    fn keyframes(&self, item: ElemId) -> Vec<(ElemId, i64, Value)> {
        self.0.keys.iter().filter(|(_, k)| k.item == item).map(|(id, k)| (*id, k.at, Value::I64(k.value))).collect()
    }
}

// The whole document.

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Doc {
    pub layers: Roots,
    pub story: Arc<RichText>,
    pub grid: Arc<Grid>,
    pub timeline: Arc<Timeline>,
    pub settings: Arc<Vec<u8>>,
    /// Derived from the story, never synced.
    pub words: usize,
}

impl Doc {
    pub fn empty() -> Self {
        let mut d = Self { story: Arc::new(RichText::plain("")), ..Self::default() };
        d.words = count_words(&d.story.text);
        d
    }
}

fn count_words(text: &str) -> usize {
    text.split_whitespace().count()
}

pub struct TestApp;

impl LiveDocument for TestApp {
    type Doc = Doc;
    const APP: &'static str = "kittest";
    const SCHEMA: u32 = 1;

    fn mappings(doc: &Doc) -> Mappings<'_> {
        Mappings::new()
            .tree("layers", LayersSrc::new(&doc.layers))
            .text("story", StorySrc(&doc.story))
            .table("grid", GridSrc(&doc.grid))
            .timeline("timeline", TimelineSrc(&doc.timeline))
            .opaque("settings", Opaque { value: Some(&*doc.settings), key: Some(Arc::as_ptr(&doc.settings) as usize) })
    }

    fn apply_remote(old: &Arc<Doc>, patch: &RemotePatch<'_>) -> Result<Arc<Doc>, KitError> {
        let mut new = (**old).clone();
        if let Some(tree) = patch.tree("layers") {
            fn build(tree: &craft_cloud_kit::TreeView<'_>, ids: Vec<ElemId>) -> Vec<Arc<Node>> {
                ids.into_iter()
                    .map(|id| {
                        let name = tree.props(id).and_then(|p| p.str("name")).unwrap_or_default();
                        Arc::new(Node { id, name, children: build(tree, tree.children(id)) })
                    })
                    .collect()
            }
            new.layers = Arc::new(build(&tree, tree.roots()));
        }
        if let Some(story) = patch.text("story") {
            new.story = Arc::new(story);
        }
        if let Some(table) = patch.table("grid") {
            let cells = table.cells().into_iter().filter_map(|(r, c, v)| v.as_str().map(|s| ((r, c), s.to_owned()))).collect();
            new.grid = Arc::new(Grid { rows: table.rows(), cols: table.columns(), cells });
        }
        if let Some(view) = patch.timeline("timeline") {
            let tracks: Vec<(ElemId, String)> =
                view.tracks().into_iter().map(|t| (t, view.track_props(t).and_then(|p| p.str("name")).unwrap_or_default())).collect();
            let mut items = BTreeMap::new();
            for (_, list) in view.items() {
                for i in list {
                    items.insert(i.id, Item { track: i.track, start: i.span.start, len: i.span.len, name: i.props.str("name").unwrap_or_default() });
                }
            }
            let mut keys = BTreeMap::new();
            for (item, list) in view.all_keyframes() {
                for (id, at, v) in list {
                    keys.insert(id, Key { item, at, value: v.as_i64().unwrap_or_default() });
                }
            }
            new.timeline = Arc::new(Timeline { tracks, items, keys });
        }
        if let Some(settings) = patch.opaque("settings") {
            new.settings = Arc::new(settings.unwrap_or_default());
        }
        Ok(Arc::new(new))
    }

    fn normalise(doc: &mut Doc) {
        // Keyframes of items that are gone, and items of tracks that are gone, aren't the app's.
        let tracks: Vec<ElemId> = doc.timeline.tracks.iter().map(|(t, _)| *t).collect();
        let tl = &doc.timeline;
        if tl.items.values().any(|i| !tracks.contains(&i.track)) || tl.keys.values().any(|k| !tl.items.contains_key(&k.item)) {
            let mut t = (**tl).clone();
            t.items.retain(|_, i| tracks.contains(&i.track));
            let items = t.items.clone();
            t.keys.retain(|_, k| items.contains_key(&k.item));
            doc.timeline = Arc::new(t);
        }
        let story = canonical(&doc.story);
        if story != *doc.story {
            doc.story = Arc::new(story);
        }
    }

    fn refresh_derived(doc: &mut Doc) {
        doc.words = count_words(&doc.story.text);
    }
}

/// Whether a model is one the app could make: unique layer IDs, canonical story, paragraphs matching, no orphans.
pub fn valid(doc: &Doc) -> Result<(), String> {
    let ids = flatten(&doc.layers);
    let mut seen = std::collections::HashSet::new();
    for (id, _) in &ids {
        if !seen.insert(*id) {
            return Err(format!("layer {id} twice"));
        }
    }
    if canonical(&doc.story) != *doc.story {
        return Err("the story isn't canonical".into());
    }
    if doc.story.paragraphs.len() != doc.story.text.matches('\n').count() + 1 {
        return Err("paragraphs don't match the breaks".into());
    }
    let tracks: Vec<ElemId> = doc.timeline.tracks.iter().map(|(t, _)| *t).collect();
    if doc.timeline.items.values().any(|i| !tracks.contains(&i.track)) {
        return Err("an item on a missing track".into());
    }
    if doc.timeline.keys.values().any(|k| !doc.timeline.items.contains_key(&k.item)) {
        return Err("a keyframe of a missing item".into());
    }
    for (r, c) in doc.grid.cells.keys() {
        if !doc.grid.rows.contains(r) || !doc.grid.cols.contains(c) {
            return Err("a cell of a missing row or column".into());
        }
    }
    if doc.words != count_words(&doc.story.text) {
        return Err("derived words are stale".into());
    }
    Ok(())
}

/// Which part an edit goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    Tree,
    Text,
    Table,
    Timeline,
    Opaque,
}

pub const SHAPES: [Shape; 5] = [Shape::Tree, Shape::Text, Shape::Table, Shape::Timeline, Shape::Opaque];

/// A random edit of `shape`, as a command of the app makes it (the result is refreshed).
pub fn edit(doc: &Doc, shape: Shape, rng: &mut Rng) -> Doc {
    let mut d = doc.clone();
    match shape {
        Shape::Tree => d.layers = Arc::new(edit_tree(&doc.layers, rng)),
        Shape::Text => d.story = Arc::new(edit_text(&doc.story, rng)),
        Shape::Table => d.grid = Arc::new(edit_grid(&doc.grid, rng)),
        Shape::Timeline => d.timeline = Arc::new(edit_timeline(&doc.timeline, rng)),
        Shape::Opaque => d.settings = Arc::new((0..rng.below(20)).map(|_| rng.next() as u8).collect()),
    }
    TestApp::refresh_derived(&mut d);
    d
}

pub fn edit_tree(roots: &[Arc<Node>], rng: &mut Rng) -> Vec<Arc<Node>> {
    let all = flatten(roots);
    let pick = |rng: &mut Rng| all.get(rng.below(all.len())).map(|(id, _)| *id);
    match if all.is_empty() { 0 } else { rng.below(4) } {
        0 => {
            let parent = if rng.chance(40) { None } else { pick(rng) };
            let n = Arc::new(Node { id: rng.id(), name: rng.word(), children: Vec::new() });
            let at = rng.below(8);
            insert(roots, parent, at, n)
        }
        1 => match pick(rng) {
            Some(id) => rename(roots, id, &rng.word()),
            None => roots.to_vec(),
        },
        2 => match pick(rng) {
            Some(id) => remove(roots, id).0,
            None => roots.to_vec(),
        },
        _ => {
            let (Some(id), parent) = (pick(rng), pick(rng)) else { return roots.to_vec() };
            let Some(node) = find(roots, id) else { return roots.to_vec() };
            let parent = parent.filter(|p| !contains(node, *p));
            let (rest, taken) = remove(roots, id);
            match taken {
                Some(n) => insert(&rest, parent, rng.below(8), n),
                None => roots.to_vec(),
            }
        }
    }
}

pub fn edit_text(t: &RichText, rng: &mut Rng) -> RichText {
    let len = t.text.chars().count();
    let range = |rng: &mut Rng| {
        let a = rng.below(len + 1);
        let b = (a + rng.below(6)).min(len);
        a..b
    };
    match rng.below(7) {
        0 | 1 => {
            let s = if rng.chance(25) { format!("{}\n{}", rng.word(), rng.word()) } else { rng.word() };
            type_at(t, rng.below(len + 1), &s)
        }
        2 => delete(t, range(rng)),
        3 => format(t, range(rng), "b", if rng.chance(70) { Some(Value::Bool(true)) } else { None }),
        4 => format(t, range(rng), "size", Some(Value::I64(8 + rng.below(4) as i64))),
        5 => {
            let paras = t.text.matches('\n').count() + 1;
            format_para(t, rng.below(paras), "h", if rng.chance(70) { Some(Value::I64(1 + rng.below(3) as i64)) } else { None })
        }
        _ => format(t, range(rng), "b", None),
    }
}

pub fn edit_grid(g: &Grid, rng: &mut Rng) -> Grid {
    let mut g = g.clone();
    match rng.below(7) {
        0 => {
            let at = rng.below(g.rows.len() + 1);
            g.rows.insert(at, rng.id());
        }
        1 => {
            let at = rng.below(g.cols.len() + 1);
            g.cols.insert(at, rng.id());
        }
        2 if !g.rows.is_empty() => {
            let r = g.rows.remove(rng.below(g.rows.len()));
            g.cells.retain(|(row, _), _| *row != r);
        }
        3 if !g.cols.is_empty() => {
            let c = g.cols.remove(rng.below(g.cols.len()));
            g.cells.retain(|(_, col), _| *col != c);
        }
        4 if !g.rows.is_empty() => {
            let r = g.rows.remove(rng.below(g.rows.len()));
            let at = rng.below(g.rows.len() + 1);
            g.rows.insert(at, r);
        }
        5 | 6 if !g.rows.is_empty() && !g.cols.is_empty() => {
            let (r, c) = (g.rows[rng.below(g.rows.len())], g.cols[rng.below(g.cols.len())]);
            if rng.chance(80) {
                g.cells.insert((r, c), rng.word());
            } else {
                g.cells.remove(&(r, c));
            }
        }
        _ => g.rows.push(rng.id()),
    }
    g
}

pub fn edit_timeline(t: &Timeline, rng: &mut Rng) -> Timeline {
    let mut t = t.clone();
    let item_ids: Vec<ElemId> = t.items.keys().copied().collect();
    let key_ids: Vec<ElemId> = t.keys.keys().copied().collect();
    match rng.below(9) {
        0 => {
            let at = rng.below(t.tracks.len() + 1);
            t.tracks.insert(at, (rng.id(), rng.word()));
        }
        1 if !t.tracks.is_empty() => {
            let (track, _) = t.tracks.remove(rng.below(t.tracks.len()));
            t.items.retain(|_, i| i.track != track);
            let items = t.items.clone();
            t.keys.retain(|_, k| items.contains_key(&k.item));
        }
        2 | 3 if !t.tracks.is_empty() => {
            let track = t.tracks[rng.below(t.tracks.len())].0;
            t.items.insert(rng.id(), Item { track, start: rng.below(1000) as i64, len: 1 + rng.below(100) as i64, name: rng.word() });
        }
        4 if !item_ids.is_empty() && !t.tracks.is_empty() => {
            let track = t.tracks[rng.below(t.tracks.len())].0;
            if let Some(i) = t.items.get_mut(&item_ids[rng.below(item_ids.len())]) {
                i.track = track;
            }
        }
        5 if !item_ids.is_empty() => {
            let start = rng.below(1000) as i64;
            if let Some(i) = t.items.get_mut(&item_ids[rng.below(item_ids.len())]) {
                i.start = start;
                i.name = format!("{}*", i.name);
            }
        }
        6 if !item_ids.is_empty() => {
            let id = item_ids[rng.below(item_ids.len())];
            t.items.remove(&id);
            t.keys.retain(|_, k| k.item != id);
        }
        7 if !item_ids.is_empty() => {
            let item = item_ids[rng.below(item_ids.len())];
            t.keys.insert(rng.id(), Key { item, at: rng.below(100) as i64, value: rng.below(10) as i64 });
        }
        8 if !key_ids.is_empty() => {
            let id = key_ids[rng.below(key_ids.len())];
            if rng.chance(50) {
                t.keys.remove(&id);
            } else if let Some(k) = t.keys.get_mut(&id) {
                k.value += 1;
            }
        }
        _ => t.tracks.push((rng.id(), rng.word())),
    }
    t
}

/// A random document: `n` edits of every shape from empty.
pub fn random_doc(rng: &mut Rng, n: usize) -> Doc {
    let mut d = Doc::empty();
    for _ in 0..n {
        let shape = SHAPES[rng.below(SHAPES.len())];
        d = edit(&d, shape, rng);
    }
    d
}
