//! How an app's model maps onto the live document: the parts it shares ([`Mappings`]), each wired to a shape (tree,
//! rich text, table, timeline, opaque value) through a source trait the app implements over its own types, and what a
//! remote change touched ([`RemotePatch`]), for the app to rebuild only that.
//!
//! Each part has a key (`layers`, `story`); its Loro containers are named after it (`layers`, `story`, `cells.rows`,
//! …), so two parts never share one.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;

use loro::LoroDoc;

use crate::props::{Props, PropsRead};
use crate::table::{TableTouch, TableView};
use crate::text::RichText;
use crate::timeline::{TimelineTouch, TimelineView};
use crate::tree::{TreeIndex, TreeTouch, TreeView};
use crate::{ElemId, KitError, Value};

/// A part key: 1 to 64 of `a-z`, `0-9`, `_` and `-`, not `root`.
pub(crate) fn check_part_key(key: &str) -> Result<(), KitError> {
    let ok =
        (1..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') && key != "root";
    if ok { Ok(()) } else { Err(KitError::Invalid(format!("{key:?} isn't a part key (1-64 of a-z, 0-9, '_', '-'; not \"root\")"))) }
}

/// A tree of elements (layers, objects, bins): `LoroTree`, a property map per node. Subtrees whose
/// [`subtree_key`](TreeSource::subtree_key) is the same in the old and new model are skipped.
pub trait TreeSource {
    /// The same value in two models when the whole tree is the same (`Arc::as_ptr` of its root list); `None`: compare.
    fn key(&self) -> Option<usize> {
        None
    }
    /// The top-level elements, in order.
    fn roots(&self) -> Vec<ElemId>;
    /// An element's children, in order (none for a leaf or an unknown ID).
    fn children(&self, id: ElemId) -> Vec<ElemId>;
    /// The same value in two models when the element and everything under it are the same (`Arc::as_ptr` as
    /// `usize`; both models are alive while the kit compares, so an address can't be reused).
    fn subtree_key(&self, _id: ElemId) -> Option<usize> {
        None
    }
    /// Writes the element's properties (only the ones that differ from what is stored are written).
    fn write(&self, id: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError>;
}

/// Character attributes of a run, or paragraph attributes: key to value (`bold` → `true`, `font` → `"Inter"`).
pub type Attrs = BTreeMap<String, Value>;

/// Rich text (a story): one `LoroText`, character attributes as marks, paragraph attributes as marks on each
/// paragraph's break (`\n`; the last paragraph's, which has none, beside it).
pub trait TextSource {
    /// The same value in two models when the text and its attributes are the same; `None`: compare.
    fn key(&self) -> Option<usize> {
        None
    }
    fn text(&self) -> &str;
    /// Runs with character attributes: byte ranges of [`TextSource::text`] on character boundaries, in order, not
    /// overlapping. What no run covers has none. (Borrowed when the app keeps them so: a keystroke copies nothing.)
    fn runs(&self) -> Cow<'_, [(Range<usize>, Attrs)]>;
    /// Each paragraph's attributes, in order: one more than the text has `\n`s (missing ones have none).
    fn paragraphs(&self) -> Cow<'_, [Attrs]>;
}

/// A table (or keyed records, with one column): movable lists of row and column IDs, and a map of cells by row and
/// column, so inserting a row doesn't renumber cells and two inserts at once keep both.
pub trait TableSource {
    /// The same value in two models when the table is the same; `None`: compare.
    fn key(&self) -> Option<usize> {
        None
    }
    fn rows(&self) -> Vec<ElemId>;
    fn columns(&self) -> Vec<ElemId>;
    fn cell(&self, row: ElemId, column: ElemId) -> Option<Value>;
    /// Every cell that holds something.
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)>;
    /// The cells changed since the old model of this change, when the app knows (GridCraft's commit does): then only
    /// those are compared. `None`: every cell is.
    fn changed_cells(&self) -> Option<Vec<(ElemId, ElemId)>> {
        None
    }
}

/// An item's place on its track, in the app's time unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: i64,
    pub len: i64,
}

/// A timeline: a movable list of tracks (each with properties), items by ID (each on a track, with a start, a length
/// and properties), and keyframes by ID (each on an item, at a time, with a value), so two keyframes added at once on
/// one item are both kept.
pub trait TimelineSource {
    /// The same value in two models when the timeline is the same; `None`: compare.
    fn key(&self) -> Option<usize> {
        None
    }
    fn tracks(&self) -> Vec<ElemId>;
    /// The same value in two models when a track and its items are the same; `None`: compare.
    fn track_key(&self, _track: ElemId) -> Option<usize> {
        None
    }
    fn write_track(&self, track: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError>;
    /// The items on a track (any order: they read back by start).
    fn items(&self, track: ElemId) -> Vec<ElemId>;
    fn span(&self, item: ElemId) -> Option<Span>;
    /// The same value in two models when an item and its keyframes are the same; `None`: compare.
    fn item_key(&self, _item: ElemId) -> Option<usize> {
        None
    }
    fn write_item(&self, item: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError>;
    /// An item's keyframes: ID, time and value.
    fn keyframes(&self, item: ElemId) -> Vec<(ElemId, i64, Value)>;
}

/// A value kept whole, last writer wins: settings, styles, plugin state ([`crate::OpaqueCodec`] encodes it).
pub trait OpaqueSource {
    /// The same value in two models when the value is the same; `None`: compare the bytes.
    fn key(&self) -> Option<usize> {
        None
    }
    /// The value's bytes (`None`: there is none), at most [`crate::MAX_VALUE_BYTES`].
    fn encode(&self) -> Result<Option<Vec<u8>>, KitError>;
}

// A borrowed source is a source: `Mappings::text("story", &doc.story)`.
impl<T: TreeSource + ?Sized> TreeSource for &T {
    fn key(&self) -> Option<usize> {
        (**self).key()
    }
    fn roots(&self) -> Vec<ElemId> {
        (**self).roots()
    }
    fn children(&self, id: ElemId) -> Vec<ElemId> {
        (**self).children(id)
    }
    fn subtree_key(&self, id: ElemId) -> Option<usize> {
        (**self).subtree_key(id)
    }
    fn write(&self, id: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        (**self).write(id, props)
    }
}

impl<T: TextSource + ?Sized> TextSource for &T {
    fn key(&self) -> Option<usize> {
        (**self).key()
    }
    fn text(&self) -> &str {
        (**self).text()
    }
    fn runs(&self) -> Cow<'_, [(Range<usize>, Attrs)]> {
        (**self).runs()
    }
    fn paragraphs(&self) -> Cow<'_, [Attrs]> {
        (**self).paragraphs()
    }
}

impl<T: TableSource + ?Sized> TableSource for &T {
    fn key(&self) -> Option<usize> {
        (**self).key()
    }
    fn rows(&self) -> Vec<ElemId> {
        (**self).rows()
    }
    fn columns(&self) -> Vec<ElemId> {
        (**self).columns()
    }
    fn cell(&self, row: ElemId, column: ElemId) -> Option<Value> {
        (**self).cell(row, column)
    }
    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        (**self).cells()
    }
    fn changed_cells(&self) -> Option<Vec<(ElemId, ElemId)>> {
        (**self).changed_cells()
    }
}

impl<T: TimelineSource + ?Sized> TimelineSource for &T {
    fn key(&self) -> Option<usize> {
        (**self).key()
    }
    fn tracks(&self) -> Vec<ElemId> {
        (**self).tracks()
    }
    fn track_key(&self, track: ElemId) -> Option<usize> {
        (**self).track_key(track)
    }
    fn write_track(&self, track: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        (**self).write_track(track, props)
    }
    fn items(&self, track: ElemId) -> Vec<ElemId> {
        (**self).items(track)
    }
    fn span(&self, item: ElemId) -> Option<Span> {
        (**self).span(item)
    }
    fn item_key(&self, item: ElemId) -> Option<usize> {
        (**self).item_key(item)
    }
    fn write_item(&self, item: ElemId, props: &mut Props<'_, '_>) -> Result<(), KitError> {
        (**self).write_item(item, props)
    }
    fn keyframes(&self, item: ElemId) -> Vec<(ElemId, i64, Value)> {
        (**self).keyframes(item)
    }
}

impl<T: OpaqueSource + ?Sized> OpaqueSource for &T {
    fn key(&self) -> Option<usize> {
        (**self).key()
    }
    fn encode(&self) -> Result<Option<Vec<u8>>, KitError> {
        (**self).encode()
    }
}

/// One shared part of the model.
pub(crate) enum Part<'a> {
    Tree(Box<dyn TreeSource + 'a>),
    Text(Box<dyn TextSource + 'a>),
    Table(Box<dyn TableSource + 'a>),
    Timeline(Box<dyn TimelineSource + 'a>),
    Opaque(Box<dyn OpaqueSource + 'a>),
}

impl Part<'_> {
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Part::Tree(_) => Kind::Tree,
            Part::Text(_) => Kind::Text,
            Part::Table(_) => Kind::Table,
            Part::Timeline(_) => Kind::Timeline,
            Part::Opaque(_) => Kind::Opaque,
        }
    }
}

/// A part's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Tree,
    Text,
    Table,
    Timeline,
    Opaque,
}

/// The parts of an app's model the live document holds, each by key. Everything else in the model (view state,
/// selection, caches, derived fields) stays the app's own.
#[derive(Default)]
pub struct Mappings<'a> {
    pub(crate) parts: Vec<(&'static str, Part<'a>)>,
}

impl<'a> Mappings<'a> {
    pub fn new() -> Self {
        Self { parts: Vec::new() }
    }

    pub fn tree(mut self, key: &'static str, source: impl TreeSource + 'a) -> Self {
        self.parts.push((key, Part::Tree(Box::new(source))));
        self
    }

    pub fn text(mut self, key: &'static str, source: impl TextSource + 'a) -> Self {
        self.parts.push((key, Part::Text(Box::new(source))));
        self
    }

    pub fn table(mut self, key: &'static str, source: impl TableSource + 'a) -> Self {
        self.parts.push((key, Part::Table(Box::new(source))));
        self
    }

    pub fn timeline(mut self, key: &'static str, source: impl TimelineSource + 'a) -> Self {
        self.parts.push((key, Part::Timeline(Box::new(source))));
        self
    }

    pub fn opaque(mut self, key: &'static str, source: impl OpaqueSource + 'a) -> Self {
        self.parts.push((key, Part::Opaque(Box::new(source))));
        self
    }

    /// The parts' keys and shapes, checked (valid, each once).
    pub(crate) fn layout(&self) -> Result<Vec<(&'static str, Kind)>, KitError> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for (key, part) in &self.parts {
            check_part_key(key)?;
            if !seen.insert(*key) {
                return Err(KitError::Invalid(format!("the part {key:?} is mapped twice")));
            }
            out.push((*key, part.kind()));
        }
        Ok(out)
    }
}

/// What a remote change (or an undo) touched, by part.
#[derive(Debug, Default, Clone)]
pub struct Touched {
    /// Everything (the document was just opened): every part reads whole.
    pub(crate) all: bool,
    pub(crate) trees: BTreeMap<String, TreeTouch>,
    pub(crate) texts: BTreeSet<String>,
    pub(crate) tables: BTreeMap<String, TableTouch>,
    pub(crate) timelines: BTreeMap<String, TimelineTouch>,
    pub(crate) opaques: BTreeSet<String>,
    /// The schema (or the save marks) changed.
    pub(crate) kit: bool,
}

impl Touched {
    pub(crate) fn everything() -> Self {
        Self { all: true, ..Self::default() }
    }

    /// Whether any part was touched.
    pub fn any(&self) -> bool {
        self.all
            || !self.trees.is_empty()
            || !self.texts.is_empty()
            || !self.tables.is_empty()
            || !self.timelines.is_empty()
            || !self.opaques.is_empty()
    }
}

/// A remote change, as the app's [`crate::LiveDocument::apply_remote`] gets it: per part, whether it was touched and
/// what of it, with the part as it is now. Parts not touched return `None`: keep them as they are.
pub struct RemotePatch<'a> {
    pub(crate) doc: &'a LoroDoc,
    pub(crate) touched: &'a Touched,
    pub(crate) trees: &'a HashMap<String, TreeIndex>,
}

impl RemotePatch<'_> {
    /// Whether this is the whole document (the first build, at open): every part is `Some`.
    pub fn is_whole(&self) -> bool {
        self.touched.all
    }

    /// The tree `key`, if the change touched it.
    pub fn tree(&self, key: &str) -> Option<TreeView<'_>> {
        let touch = if self.touched.all { TreeTouch::whole() } else { self.touched.trees.get(key)?.clone() };
        let index = self.trees.get(key)?;
        Some(TreeView::new(self.doc, key, index, touch))
    }

    /// The rich text `key` as it is now, if the change touched it.
    pub fn text(&self, key: &str) -> Option<RichText> {
        if !self.touched.all && !self.touched.texts.contains(key) {
            return None;
        }
        Some(crate::text::read(self.doc, key))
    }

    /// The table `key`, if the change touched it.
    pub fn table(&self, key: &str) -> Option<TableView<'_>> {
        let touch = if self.touched.all { TableTouch::whole() } else { self.touched.tables.get(key)?.clone() };
        Some(TableView::new(self.doc, key, touch))
    }

    /// The timeline `key`, if the change touched it.
    pub fn timeline(&self, key: &str) -> Option<TimelineView<'_>> {
        let touch = if self.touched.all { TimelineTouch::whole() } else { self.touched.timelines.get(key)?.clone() };
        Some(TimelineView::new(self.doc, key, touch))
    }

    /// The opaque value `key` as it is now (`Some(None)`: it was removed), if the change touched it.
    pub fn opaque(&self, key: &str) -> Option<Option<Vec<u8>>> {
        if !self.touched.all && !self.touched.opaques.contains(key) {
            return None;
        }
        Some(crate::opaque::read(self.doc, key))
    }
}

/// Reads back an element's properties: for tree nodes, tracks and items ([`TreeView::props`] and the others).
pub(crate) fn props_of(map: loro::LoroMap) -> PropsRead {
    PropsRead { map }
}
