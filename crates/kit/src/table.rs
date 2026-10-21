//! Tables: `<key>.rows` and `<key>.cols` (movable lists of IDs) and `<key>.cells` (a map from `row:column` to the
//! cell). A cell whose row or column is gone (deleted on one machine while edited on another) is never read back, and
//! goes with the next change to its row or column.
//!
//! A cell is a value, or, for a table whose source says so ([`TableSource::cell_maps`]), a property map of its own
//! (a mergeable child of the cell map) holding values and stories, written like a tree node's. A table can also sit
//! inside a tree node's (or a cell's) map ([`crate::Props::table`]): the same three containers, as mergeable children
//! `rows`, `cols` and `cells` of the map under the codec's key.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use loro::{Container, LoroDoc, LoroMap, LoroMovableList, StyleConfigMap, TextDelta, ValueOrContainer};

use crate::mapping::TableSource;
use crate::props::{Props, PropsRead};
use crate::text::RichText;
use crate::writer::Writer;
use crate::{ElemId, KitError, Value};

pub(crate) fn rows_name(key: &str) -> String {
    format!("{key}.rows")
}

pub(crate) fn cols_name(key: &str) -> String {
    format!("{key}.cols")
}

pub(crate) fn cells_name(key: &str) -> String {
    format!("{key}.cells")
}

pub(crate) fn cell_key(row: ElemId, col: ElemId) -> String {
    format!("{}:{}", row.hex(), col.hex())
}

pub(crate) fn parse_cell_key(key: &str) -> Option<(ElemId, ElemId)> {
    let (r, c) = key.split_once(':')?;
    Some((ElemId::from_hex(r)?, ElemId::from_hex(c)?))
}

#[derive(Clone)]
pub(crate) struct Containers {
    rows: LoroMovableList,
    cols: LoroMovableList,
    cells: LoroMap,
}

fn containers(doc: &LoroDoc, key: &str) -> Containers {
    Containers { rows: doc.get_movable_list(rows_name(key)), cols: doc.get_movable_list(cols_name(key)), cells: doc.get_map(cells_name(key)) }
}

/// A nested table's containers, made where missing.
fn nested_made(map: &LoroMap) -> Result<Containers, KitError> {
    Ok(Containers {
        rows: map.ensure_mergeable_movable_list("rows")?,
        cols: map.ensure_mergeable_movable_list("cols")?,
        cells: map.ensure_mergeable_map("cells")?,
    })
}

fn set_cell(w: &mut Writer<'_>, cells: &LoroMap, row: ElemId, col: ElemId, value: Option<Value>) -> Result<(), KitError> {
    let key = cell_key(row, col);
    match value {
        Some(v) => w.set(cells, &key, &v).map(|_| ()),
        None => w.delete(cells, &key).map(|_| ()),
    }
}

/// Writes one map cell: its property map made if missing, then the source's properties and stories.
fn write_map_cell(
    w: &mut Writer<'_>,
    cells: &LoroMap,
    row: ElemId,
    col: ElemId,
    new: &dyn TableSource,
    styles: &mut StyleConfigMap,
) -> Result<(), KitError> {
    let key = cell_key(row, col);
    let map = match cells.get(&key) {
        Some(ValueOrContainer::Container(Container::Map(map))) => map,
        _ => {
            w.spend(crate::writer::OP_OVERHEAD + key.len())?;
            cells.ensure_mergeable_map(&key)?
        }
    };
    let mut props = Props::new(map, w, styles);
    new.write_cell(row, col, &mut props)?;
    props.finish()
}

/// Writes what changed from `old` to `new` (everything, when `old` is `None`) into the root table `key`.
pub(crate) fn write(
    w: &mut Writer<'_>,
    key: &str,
    old: Option<&dyn TableSource>,
    new: &dyn TableSource,
    styles: &mut StyleConfigMap,
) -> Result<(), KitError> {
    if let (Some(old), Some(k)) = (old, new.key())
        && old.key() == Some(k)
    {
        return Ok(());
    }
    let c = containers(w.doc, key);
    write_in(w, &c, old, new, styles)
}

/// Writes `new` into the table under `map` (a node's or a cell's), against what it holds.
pub(crate) fn write_nested(w: &mut Writer<'_>, map: &LoroMap, new: &dyn TableSource, styles: &mut StyleConfigMap) -> Result<(), KitError> {
    let c = nested_made(map)?;
    write_in(w, &c, None, new, styles)
}

fn write_in(
    w: &mut Writer<'_>,
    c: &Containers,
    old: Option<&dyn TableSource>,
    new: &dyn TableSource,
    styles: &mut StyleConfigMap,
) -> Result<(), KitError> {
    let stored_cols = crate::list::ids(&c.cols);
    let stored_rows = crate::list::ids(&c.rows);
    let (new_rows, new_cols) = (new.rows(), new.columns());
    let removed_rows = crate::list::sync(w, &c.rows, &new_rows)?;
    let removed_cols = crate::list::sync(w, &c.cols, &new_cols)?;
    // The cells of removed rows and columns.
    for row in &removed_rows {
        for col in &stored_cols {
            set_cell(w, &c.cells, *row, *col, None)?;
        }
    }
    for col in &removed_cols {
        for row in &stored_rows {
            set_cell(w, &c.cells, *row, *col, None)?;
        }
    }
    if new.cell_maps() {
        // Every cell of the table is a map: each written unless the source says it is unchanged.
        let hint: Option<HashSet<(ElemId, ElemId)>> = match (old, new.changed_cells()) {
            (Some(_), Some(hint)) => Some(hint.into_iter().collect()),
            _ => None,
        };
        for row in &new_rows {
            for col in &new_cols {
                if let Some(hint) = &hint
                    && !hint.contains(&(*row, *col))
                {
                    continue;
                }
                let unchanged = old.is_some_and(|old| {
                    let k = old.cell_key(*row, *col);
                    k.is_some() && k == new.cell_key(*row, *col)
                });
                if !unchanged {
                    write_map_cell(w, &c.cells, *row, *col, new, styles)?;
                }
            }
        }
        if old.is_none() {
            sweep(w, c, &new_rows, &new_cols, |_, _| false)?;
        }
        return Ok(());
    }
    match (old, new.changed_cells()) {
        (Some(_), Some(hint)) => {
            for (row, col) in hint {
                set_cell(w, &c.cells, row, col, new.cell(row, col))?;
            }
        }
        (Some(old), None) => {
            let before: HashMap<(ElemId, ElemId), Value> = old.cells().into_iter().map(|(r, c, v)| ((r, c), v)).collect();
            let mut now: HashSet<(ElemId, ElemId)> = HashSet::new();
            for (row, col, value) in new.cells() {
                now.insert((row, col));
                if before.get(&(row, col)) != Some(&value) {
                    set_cell(w, &c.cells, row, col, Some(value))?;
                }
            }
            for (row, col) in before.keys() {
                if !now.contains(&(*row, *col)) {
                    set_cell(w, &c.cells, *row, *col, None)?;
                }
            }
        }
        (None, _) => {
            let mut now: HashSet<(ElemId, ElemId)> = HashSet::new();
            for (row, col, value) in new.cells() {
                now.insert((row, col));
                set_cell(w, &c.cells, row, col, Some(value))?;
            }
            // Against what the table holds: a cell emptied since goes.
            sweep(w, c, &new_rows, &new_cols, |r, col| !now.contains(&(r, col)))?;
        }
    }
    Ok(())
}

/// Deletes the stored cells of rows or columns that aren't the table's (orphans of a merge), and those of its own
/// cells `stale` says go.
fn sweep(w: &mut Writer<'_>, c: &Containers, rows: &[ElemId], cols: &[ElemId], stale: impl Fn(ElemId, ElemId) -> bool) -> Result<(), KitError> {
    let rows: HashSet<ElemId> = rows.iter().copied().collect();
    let cols: HashSet<ElemId> = cols.iter().copied().collect();
    let mut gone = Vec::new();
    c.cells.for_each(|key, _| match parse_cell_key(key) {
        Some((r, col)) if rows.contains(&r) && cols.contains(&col) && !stale(r, col) => {}
        _ => gone.push(key.to_owned()),
    });
    for key in gone {
        w.delete(&c.cells, &key)?;
    }
    Ok(())
}

/// What a remote change touched of a table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableTouch {
    /// Everything (read it whole).
    pub all: bool,
    pub rows: bool,
    pub columns: bool,
    /// The cells written or removed (a map cell's properties or stories changed).
    pub cells: BTreeSet<(ElemId, ElemId)>,
    /// The stories of map cells that changed: row, column and the story's key.
    pub stories: BTreeSet<(ElemId, ElemId, String)>,
}

impl TableTouch {
    pub(crate) fn whole() -> Self {
        Self { all: true, ..Self::default() }
    }
}

/// Loro's text deltas of the cells' stories, by row, column and key, in the order they came.
pub(crate) type CellDeltas = BTreeMap<(ElemId, ElemId, String), Vec<Vec<TextDelta>>>;

/// A table as stored, after a remote change.
pub struct TableView<'a> {
    c: Containers,
    touch: TableTouch,
    deltas: Option<&'a CellDeltas>,
}

impl<'a> TableView<'a> {
    pub(crate) fn new(doc: &'a LoroDoc, key: &str, touch: TableTouch, deltas: Option<&'a CellDeltas>) -> Self {
        Self { c: containers(doc, key), touch, deltas }
    }

    /// The table under a node's or cell's map, read whole.
    pub(crate) fn nested(map: &LoroMap) -> TableView<'static> {
        // Read without making anything: missing containers read as empty, detached ones.
        let get_list = |k: &str| match map.get(k) {
            Some(ValueOrContainer::Container(Container::MovableList(l))) => l,
            _ => LoroMovableList::new(),
        };
        let cells = match map.get("cells") {
            Some(ValueOrContainer::Container(Container::Map(m))) => m,
            _ => LoroMap::new(),
        };
        TableView { c: Containers { rows: get_list("rows"), cols: get_list("cols"), cells }, touch: TableTouch::whole(), deltas: None }
    }

    pub fn touch(&self) -> &TableTouch {
        &self.touch
    }

    /// The rows, in order, each once.
    pub fn rows(&self) -> Vec<ElemId> {
        crate::list::ids(&self.c.rows)
    }

    /// The columns, in order, each once.
    pub fn columns(&self) -> Vec<ElemId> {
        crate::list::ids(&self.c.cols)
    }

    /// A cell's value (`None` when empty, unreadable, or a map cell).
    pub fn cell(&self, row: ElemId, col: ElemId) -> Option<Value> {
        match self.c.cells.get(&cell_key(row, col))? {
            ValueOrContainer::Value(v) => Value::from_loro(&v),
            ValueOrContainer::Container(_) => None,
        }
    }

    /// A map cell's properties and stories (`None` when it has none: a cell a merge left out, which the app's
    /// `normalise` makes again).
    pub fn cell_props(&self, row: ElemId, col: ElemId) -> Option<PropsRead> {
        match self.c.cells.get(&cell_key(row, col))? {
            ValueOrContainer::Container(Container::Map(map)) => Some(PropsRead { map }),
            ValueOrContainer::Value(_) | ValueOrContainer::Container(_) => None,
        }
    }

    /// A map cell's story `key`, patched from `old` (the app's text of it before the change) when the remote change's
    /// deltas fit it, else read whole.
    pub fn cell_text(&self, row: ElemId, col: ElemId, key: &str, old: Option<&dyn crate::TextSource>) -> Option<RichText> {
        let props = self.cell_props(row, col)?;
        let deltas = self.deltas.and_then(|d| d.get(&(row, col, key.to_owned())));
        props.text_patched(key, old, deltas)
    }

    /// Every readable cell value of a row and column that are there.
    pub fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        let rows: HashSet<ElemId> = self.rows().into_iter().collect();
        let cols: HashSet<ElemId> = self.columns().into_iter().collect();
        let mut out = Vec::new();
        self.c.cells.for_each(|key, value| {
            let Some((r, c)) = parse_cell_key(key) else { return };
            if !rows.contains(&r) || !cols.contains(&c) {
                return;
            }
            if let ValueOrContainer::Value(v) = value
                && let Some(v) = Value::from_loro(&v)
            {
                out.push((r, c, v));
            }
        });
        out.sort_by_key(|a| (a.0, a.1));
        out
    }
}
