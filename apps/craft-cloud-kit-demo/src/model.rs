//! The demo's document: a story (rich text: bold runs, headings) and a tree of layers beside it, held copy-on-write
//! behind `Arc`s as the Craft apps hold theirs; its adapter for the kit ([`DemoApp`]); its edits; and its file (JSON).

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use craft_cloud_kit::{Attrs, ElemId, KitError, LiveDocument, Mappings, Props, RemotePatch, RichText, TextSource, TreeSource, Value};
use serde::{Deserialize, Serialize};

/// The character attribute for bold, and the paragraph attribute for a heading.
pub const BOLD: &str = "b";
pub const HEADING: &str = "h";

#[derive(Debug, Clone, PartialEq)]
pub struct Layer {
    pub id: ElemId,
    pub name: String,
    pub children: Vec<Arc<Layer>>,
}

pub type Layers = Arc<Vec<Arc<Layer>>>;

#[derive(Debug, Clone, PartialEq)]
pub struct DemoDoc {
    pub story: Arc<RichText>,
    pub layers: Layers,
    /// Derived (never synced): the story's word count.
    pub words: usize,
}

impl Default for DemoDoc {
    fn default() -> Self {
        Self { story: Arc::new(RichText::plain("")), layers: Arc::new(Vec::new()), words: 0 }
    }
}

// The adapter.

/// The demo's mapping: the tree `layers` (each layer's name) and the rich text `story`.
pub struct DemoApp;

/// The layer tree as a tree source; its index is made only if the tree is compared (a keystroke in the story skips it).
pub struct LayersSrc<'a> {
    roots: &'a Layers,
    index: OnceCell<HashMap<ElemId, &'a Arc<Layer>>>,
}

impl<'a> LayersSrc<'a> {
    pub fn new(roots: &'a Layers) -> Self {
        Self { roots, index: OnceCell::new() }
    }

    fn index(&self) -> &HashMap<ElemId, &'a Arc<Layer>> {
        self.index.get_or_init(|| {
            let mut index = HashMap::new();
            let mut stack: Vec<&'a Arc<Layer>> = self.roots.iter().collect();
            while let Some(n) = stack.pop() {
                index.insert(n.id, n);
                stack.extend(n.children.iter());
            }
            index
        })
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
        self.index().get(&id).map(|n| n.children.iter().map(|c| c.id).collect()).unwrap_or_default()
    }
    fn subtree_key(&self, id: ElemId) -> Option<usize> {
        self.index().get(&id).map(|n| Arc::as_ptr(n) as usize)
    }
    fn write(&self, id: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        if let Some(n) = self.index().get(&id) {
            props.set("name", n.name.as_str())?;
        }
        Ok(())
    }
}

/// The story as a text source (keyed by its `Arc`: unchanged, it is skipped).
pub struct StorySrc<'a>(pub &'a Arc<RichText>);

impl TextSource for StorySrc<'_> {
    fn key(&self) -> Option<usize> {
        Some(Arc::as_ptr(self.0) as usize)
    }
    fn text(&self) -> &str {
        &self.0.text
    }
    fn runs(&self) -> Cow<'_, [(Range<usize>, Attrs)]> {
        Cow::Borrowed(&self.0.runs)
    }
    fn paragraphs(&self) -> Cow<'_, [Attrs]> {
        Cow::Borrowed(&self.0.paragraphs)
    }
}

/// A layer from the tree as stored (its children too).
fn build(tree: &craft_cloud_kit::TreeView<'_>, ids: Vec<ElemId>) -> Vec<Arc<Layer>> {
    ids.into_iter()
        .map(|id| {
            let name = tree.props(id).and_then(|p| p.str("name")).unwrap_or_default();
            Arc::new(Layer { id, name, children: build(tree, tree.children(id)) })
        })
        .collect()
}

impl LiveDocument for DemoApp {
    type Doc = DemoDoc;
    const APP: &'static str = "kitdemo";
    const SCHEMA: u32 = 1;

    fn mappings(doc: &DemoDoc) -> Mappings<'_> {
        Mappings::new().tree("layers", LayersSrc::new(&doc.layers)).text("story", StorySrc(&doc.story))
    }

    fn apply_remote(old: &Arc<DemoDoc>, patch: &RemotePatch<'_>) -> Result<Arc<DemoDoc>, KitError> {
        let mut new = (**old).clone();
        if let Some(tree) = patch.tree("layers") {
            if tree.touch().structure || tree.touch().all {
                new.layers = Arc::new(build(&tree, tree.roots()));
            } else {
                // Only names changed: just those layers (and the path to them) are new.
                let mut layers = (*new.layers).clone();
                for id in &tree.touch().nodes {
                    let name = tree.props(*id).and_then(|p| p.str("name")).unwrap_or_default();
                    layers = rename(&layers, *id, &name);
                }
                new.layers = Arc::new(layers);
            }
        }
        if let Some(story) = patch.text("story") {
            new.story = Arc::new(story);
        }
        Ok(Arc::new(new))
    }

    fn normalise(doc: &mut DemoDoc) {
        // A layer with no name (two machines cleared it, say) gets one: what the app itself does.
        let mut unnamed = Vec::new();
        let mut stack: Vec<&Arc<Layer>> = doc.layers.iter().collect();
        while let Some(n) = stack.pop() {
            if n.name.trim().is_empty() {
                unnamed.push(n.id);
            }
            stack.extend(n.children.iter());
        }
        if !unnamed.is_empty() {
            let mut layers = (*doc.layers).clone();
            for id in unnamed {
                layers = rename(&layers, id, "Untitled layer");
            }
            doc.layers = Arc::new(layers);
        }
    }

    fn refresh_derived(doc: &mut DemoDoc) {
        doc.words = doc.story.text.split_whitespace().count();
    }
}

// The layer edits, copy-on-write: what didn't change stays shared.

fn contains(node: &Layer, id: ElemId) -> bool {
    node.id == id || node.children.iter().any(|c| contains(c, id))
}

pub fn find(roots: &[Arc<Layer>], id: ElemId) -> Option<&Arc<Layer>> {
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

/// Where a layer is: its parent (`None` at the top) and its place there.
pub fn place(roots: &[Arc<Layer>], id: ElemId) -> Option<(Option<ElemId>, usize)> {
    if let Some(i) = roots.iter().position(|n| n.id == id) {
        return Some((None, i));
    }
    for n in roots {
        if let Some(i) = n.children.iter().position(|c| c.id == id) {
            return Some((Some(n.id), i));
        }
        if let Some(found) = place(&n.children, id) {
            return Some(found);
        }
    }
    None
}

pub fn remove(roots: &[Arc<Layer>], id: ElemId) -> (Vec<Arc<Layer>>, Option<Arc<Layer>>) {
    let mut taken = None;
    let mut out = Vec::with_capacity(roots.len());
    for n in roots {
        if n.id == id {
            taken = Some(n.clone());
        } else if taken.is_none() && contains(n, id) {
            let (children, t) = remove(&n.children, id);
            taken = t;
            out.push(Arc::new(Layer { id: n.id, name: n.name.clone(), children }));
        } else {
            out.push(n.clone());
        }
    }
    (out, taken)
}

pub fn insert(roots: &[Arc<Layer>], parent: Option<ElemId>, at: usize, node: Arc<Layer>) -> Vec<Arc<Layer>> {
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
                    Arc::new(Layer { id: n.id, name: n.name.clone(), children })
                } else if contains(n, p) {
                    Arc::new(Layer { id: n.id, name: n.name.clone(), children: insert(&n.children, parent, at, node.clone()) })
                } else {
                    n.clone()
                }
            })
            .collect(),
    }
}

pub fn rename(roots: &[Arc<Layer>], id: ElemId, name: &str) -> Vec<Arc<Layer>> {
    roots
        .iter()
        .map(|n| {
            if n.id == id {
                Arc::new(Layer { id, name: name.to_owned(), children: n.children.clone() })
            } else if contains(n, id) {
                Arc::new(Layer { id: n.id, name: n.name.clone(), children: rename(&n.children, id, name) })
            } else {
                n.clone()
            }
        })
        .collect()
}

/// A layer moved up (`-1`) or down (`1`) among its siblings.
pub fn shift(roots: &[Arc<Layer>], id: ElemId, by: isize) -> Vec<Arc<Layer>> {
    let Some((parent, at)) = place(roots, id) else { return roots.to_vec() };
    let to = at.saturating_add_signed(by);
    let (rest, taken) = remove(roots, id);
    match taken {
        Some(n) => insert(&rest, parent, to, n),
        None => roots.to_vec(),
    }
}

/// A layer made the last child of the sibling before it.
pub fn indent(roots: &[Arc<Layer>], id: ElemId) -> Vec<Arc<Layer>> {
    let Some((parent, at)) = place(roots, id) else { return roots.to_vec() };
    if at == 0 {
        return roots.to_vec();
    }
    let siblings: Vec<ElemId> = match parent {
        None => roots.iter().map(|n| n.id).collect(),
        Some(p) => find(roots, p).map(|n| n.children.iter().map(|c| c.id).collect()).unwrap_or_default(),
    };
    let Some(&before) = siblings.get(at - 1) else { return roots.to_vec() };
    let (rest, taken) = remove(roots, id);
    match taken {
        Some(n) => insert(&rest, Some(before), usize::MAX, n),
        None => roots.to_vec(),
    }
}

/// A layer moved out of its parent, right after it.
pub fn outdent(roots: &[Arc<Layer>], id: ElemId) -> Vec<Arc<Layer>> {
    let Some((Some(parent), _)) = place(roots, id) else { return roots.to_vec() };
    let Some((grand, at)) = place(roots, parent) else { return roots.to_vec() };
    let (rest, taken) = remove(roots, id);
    match taken {
        Some(n) => insert(&rest, grand, at + 1, n),
        None => roots.to_vec(),
    }
}

/// Every layer, depth first, with its depth.
pub fn flatten(roots: &[Arc<Layer>]) -> Vec<(usize, Arc<Layer>)> {
    let mut out = Vec::new();
    let mut stack: Vec<(usize, &Arc<Layer>)> = roots.iter().rev().map(|n| (0, n)).collect();
    while let Some((depth, n)) = stack.pop() {
        out.push((depth, n.clone()));
        stack.extend(n.children.iter().rev().map(|c| (depth + 1, c)));
    }
    out
}

// The story edits.

/// Merges adjacent runs with the same attributes and drops empty ones.
fn tidy(runs: Vec<(Range<usize>, Attrs)>) -> Vec<(Range<usize>, Attrs)> {
    let mut out: Vec<(Range<usize>, Attrs)> = Vec::with_capacity(runs.len());
    for (r, a) in runs {
        if r.start >= r.end || a.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((last, la)) if last.end == r.start && *la == a => last.end = r.end,
            _ => out.push((r, a)),
        }
    }
    out
}

fn prefix(a: &str, b: &str) -> usize {
    let mut i = a.as_bytes().iter().zip(b.as_bytes()).take_while(|(x, y)| x == y).count();
    while i > 0 && !a.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn suffix(a: &str, b: &str, max: usize) -> usize {
    let mut n = a.as_bytes().iter().rev().zip(b.as_bytes().iter().rev()).take(max).take_while(|(x, y)| x == y).count();
    while n > 0 && !a.is_char_boundary(a.len() - n) {
        n -= 1;
    }
    n
}

/// The story with its text replaced by `text` (as the text box edited it): typed text takes the attributes of the
/// character before it, a split paragraph's new part takes its attributes, and joined paragraphs keep the first's.
pub fn edit_text(old: &RichText, text: &str) -> RichText {
    let p = prefix(&old.text, text);
    let s = suffix(&old.text, text, old.text.len().min(text.len()) - p);
    let (old_end, new_end) = (old.text.len() - s, text.len() - s);
    let inserted = new_end - p;
    let mut runs = Vec::with_capacity(old.runs.len() + 1);
    let mut inherited = Attrs::new();
    for (r, a) in &old.runs {
        if r.start < p && p <= r.end {
            inherited = a.clone();
        }
        if r.start < p {
            runs.push((r.start..r.end.min(p), a.clone()));
        }
        if r.end > old_end {
            runs.push((r.start.max(old_end) - old_end + new_end..r.end - old_end + new_end, a.clone()));
        }
    }
    if inserted > 0 && !inherited.is_empty() {
        runs.push((p..new_end, inherited));
    }
    runs.sort_by_key(|(r, _)| r.start);
    let before = old.text.get(..p).map_or(0, |t| t.matches('\n').count());
    let joined = old.text.get(p..old_end).map_or(0, |t| t.matches('\n').count());
    let split = text.get(p..new_end).map_or(0, |t| t.matches('\n').count());
    let mut paragraphs = old.paragraphs.clone();
    paragraphs.resize(old.text.matches('\n').count() + 1, Attrs::new());
    let keep = paragraphs.get(before).cloned().unwrap_or_default();
    let end = (before + 1 + joined).min(paragraphs.len());
    paragraphs.drain((before + 1).min(end)..end);
    for _ in 0..split {
        paragraphs.insert(before, keep.clone());
    }
    RichText { text: text.to_owned(), runs: tidy(runs), paragraphs }
}

/// The story with attribute `key` set (or removed) on bytes `range`.
pub fn format(old: &RichText, range: Range<usize>, key: &str, value: Option<Value>) -> RichText {
    let (a, b) = (range.start.min(old.text.len()), range.end.min(old.text.len()));
    let mut runs = Vec::with_capacity(old.runs.len() + 2);
    let mut at = 0;
    let mut pieces: Vec<(Range<usize>, Attrs)> = Vec::new();
    for (r, attrs) in &old.runs {
        if r.start > at {
            pieces.push((at..r.start, Attrs::new()));
        }
        pieces.push((r.clone(), attrs.clone()));
        at = r.end;
    }
    if at < old.text.len() {
        pieces.push((at..old.text.len(), Attrs::new()));
    }
    for (r, attrs) in pieces {
        for (x, y, inside) in [(r.start, r.end.min(a), false), (r.start.max(a), r.end.min(b), true), (r.start.max(b), r.end, false)] {
            if x >= y {
                continue;
            }
            let mut attrs = attrs.clone();
            if inside {
                match &value {
                    Some(v) => attrs.insert(key.to_owned(), v.clone()),
                    None => attrs.remove(key),
                };
            }
            runs.push((x..y, attrs));
        }
    }
    RichText { text: old.text.clone(), runs: tidy(runs), paragraphs: old.paragraphs.clone() }
}

/// Whether every byte of `range` has attribute `key`.
pub fn has(story: &RichText, range: Range<usize>, key: &str) -> bool {
    !range.is_empty()
        && story
            .runs
            .iter()
            .filter(|(_, a)| a.contains_key(key))
            .map(|(r, _)| r.end.min(range.end).saturating_sub(r.start.max(range.start)))
            .sum::<usize>()
            >= range.len()
}

/// The paragraph that byte `at` is in.
pub fn paragraph_at(story: &RichText, at: usize) -> usize {
    story.text.get(..at.min(story.text.len())).map_or(0, |t| t.matches('\n').count())
}

/// The story with paragraph `para`'s heading set (or removed).
pub fn heading(old: &RichText, para: usize, level: Option<i64>) -> RichText {
    let mut paragraphs = old.paragraphs.clone();
    paragraphs.resize(old.text.matches('\n').count() + 1, Attrs::new());
    if let Some(p) = paragraphs.get_mut(para) {
        match level {
            Some(l) => p.insert(HEADING.to_owned(), Value::I64(l)),
            None => p.remove(HEADING),
        };
    }
    RichText { text: old.text.clone(), runs: old.runs.clone(), paragraphs }
}

// The file: `.kitdemo`, JSON.

/// The file format's name and version.
pub const FORMAT: &str = "craft-cloud-kit-demo";
pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
struct FileDoc {
    format: String,
    version: u32,
    story: FileStory,
    layers: Vec<FileLayer>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileStory {
    text: String,
    /// Bold byte ranges.
    bold: Vec<[usize; 2]>,
    /// Paragraphs that are headings: index and level.
    headings: Vec<[i64; 2]>,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileLayer {
    id: String,
    name: String,
    #[serde(default)]
    children: Vec<FileLayer>,
}

/// The document as the file's bytes.
pub fn to_file(doc: &DemoDoc) -> Vec<u8> {
    fn layer(l: &Layer) -> FileLayer {
        FileLayer { id: l.id.hex(), name: l.name.clone(), children: l.children.iter().map(|c| layer(c)).collect() }
    }
    let story = FileStory {
        text: doc.story.text.clone(),
        bold: doc.story.runs.iter().filter(|(_, a)| a.contains_key(BOLD)).map(|(r, _)| [r.start, r.end]).collect(),
        headings: doc
            .story
            .paragraphs
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.get(HEADING).and_then(Value::as_i64).map(|l| [i as i64, l]))
            .collect(),
    };
    let file = FileDoc { format: FORMAT.into(), version: FORMAT_VERSION, story, layers: doc.layers.iter().map(|l| layer(l)).collect() };
    serde_json::to_vec_pretty(&file).unwrap_or_default()
}

/// A document from a file's bytes; an error says why it doesn't read.
pub fn from_file(bytes: &[u8]) -> Result<DemoDoc, String> {
    let file: FileDoc = serde_json::from_slice(bytes).map_err(|e| format!("not a demo file: {e}"))?;
    if file.format != FORMAT || file.version > FORMAT_VERSION {
        return Err(format!("a {} file of version {}: this demo reads {FORMAT} {FORMAT_VERSION}", file.format, file.version));
    }
    fn layer(f: FileLayer) -> Result<Arc<Layer>, String> {
        let id = ElemId::from_hex(&f.id).ok_or_else(|| format!("{:?} isn't a layer ID", f.id))?;
        Ok(Arc::new(Layer { id, name: f.name, children: f.children.into_iter().map(layer).collect::<Result<_, _>>()? }))
    }
    let mut story = RichText::plain(&file.story.text);
    for [a, b] in file.story.bold {
        if story.text.is_char_boundary(a.min(story.text.len())) && story.text.is_char_boundary(b.min(story.text.len())) {
            story = format(&story, a..b, BOLD, Some(Value::Bool(true)));
        }
    }
    for [para, level] in file.story.headings {
        story = heading(&story, usize::try_from(para).unwrap_or(usize::MAX), Some(level));
    }
    let mut doc = DemoDoc { story: Arc::new(story), layers: Arc::new(file.layers.into_iter().map(layer).collect::<Result<_, _>>()?), words: 0 };
    DemoApp::refresh_derived(&mut doc);
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_splitting_and_joining_keep_the_attributes() {
        let s = RichText::plain("Hello world");
        let s = format(&s, 0..5, BOLD, Some(Value::Bool(true)));
        let s = heading(&s, 0, Some(1));
        let typed = edit_text(&s, "Hellooo world");
        assert!(has(&typed, 0..7, BOLD) && !has(&typed, 7..8, BOLD), "{:?}", typed.runs);
        let split = edit_text(&typed, "Hel\nlooo world");
        assert_eq!(split.paragraphs.len(), 2);
        assert!(split.paragraphs.iter().all(|p| p.get(HEADING) == Some(&Value::I64(1))));
        let joined = edit_text(&split, "Hellooo world");
        assert_eq!(joined.paragraphs.len(), 1);
        let unbold = format(&joined, 0..3, BOLD, None);
        assert!(!has(&unbold, 0..3, BOLD) && has(&unbold, 3..7, BOLD));
        assert_eq!(paragraph_at(&split, 5), 1);
    }

    #[test]
    fn layers_move_and_the_file_reads_back() {
        let (a, b, c) = (ElemId(1), ElemId(2), ElemId(3));
        let l = |id, name: &str| Arc::new(Layer { id, name: name.into(), children: Vec::new() });
        let roots = vec![l(a, "a"), l(b, "b"), l(c, "c")];
        let roots = indent(&roots, b);
        assert_eq!(place(&roots, b), Some((Some(a), 0)));
        let roots = outdent(&roots, b);
        assert_eq!(place(&roots, b), Some((None, 1)));
        let roots = shift(&roots, c, -1);
        assert_eq!(flatten(&roots).iter().map(|(_, n)| n.id).collect::<Vec<_>>(), vec![a, c, b]);
        let story = heading(&format(&RichText::plain("Title\nbody"), 0..5, BOLD, Some(Value::Bool(true))), 0, Some(1));
        let mut doc = DemoDoc { story: Arc::new(story), layers: Arc::new(rename(&roots, c, "renamed")), words: 0 };
        DemoApp::refresh_derived(&mut doc);
        let back = from_file(&to_file(&doc)).unwrap();
        assert_eq!(back, doc);
        assert!(from_file(b"{}").is_err());
    }
}
