//! Tables: `<key>.rows` and `<key>.cols` (movable lists of IDs) and `<key>.cells` (a map from `row:column` to the
//! cell's value). A cell whose row or column is gone (deleted on one machine while edited on another) is never read
//! back, and goes with the next change to its row or column.

use std::collections::{BTreeSet, HashMap, HashSet};

use loro::{LoroDoc, LoroMap, LoroMovableList, ValueOrContainer};

use crate::mapping::TableSource;
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

struct Containers {
    rows: LoroMovableList,
    cols: LoroMovableList,
    cells: LoroMap,
}

fn containers(doc: &LoroDoc, key: &str) -> Containers {
    Containers { rows: doc.get_movable_list(rows_name(key)), cols: doc.get_movable_list(cols_name(key)), cells: doc.get_map(cells_name(key)) }
}

fn set_cell(w: &mut Writer<'_>, cells: &LoroMap, row: ElemId, col: ElemId, value: Option<Value>) -> Result<(), KitError> {
    let key = cell_key(row, col);
    match value {
        Some(v) => w.set(cells, &key, &v).map(|_| ()),
        None => w.delete(cells, &key).map(|_| ()),
    }
}

/// Writes what changed from `old` to `new` (everything, when `old` is `None`).
pub(crate) fn write(w: &mut Writer<'_>, key: &str, old: Option<&dyn TableSource>, new: &dyn TableSource) -> Result<(), KitError> {
    if let (Some(old), Some(k)) = (old, new.key())
        && old.key() == Some(k)
    {
        return Ok(());
    }
    let c = containers(w.doc, key);
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
            for (row, col, value) in new.cells() {
                set_cell(w, &c.cells, row, col, Some(value))?;
            }
        }
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
    /// The cells written or removed.
    pub cells: BTreeSet<(ElemId, ElemId)>,
}

impl TableTouch {
    pub(crate) fn whole() -> Self {
        Self { all: true, ..Self::default() }
    }
}

/// A table as stored, after a remote change.
pub struct TableView<'a> {
    c: Containers,
    touch: TableTouch,
    _doc: std::marker::PhantomData<&'a LoroDoc>,
}

impl<'a> TableView<'a> {
    pub(crate) fn new(doc: &'a LoroDoc, key: &str, touch: TableTouch) -> Self {
        Self { c: containers(doc, key), touch, _doc: std::marker::PhantomData }
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

    /// A cell's value (`None` when empty or unreadable).
    pub fn cell(&self, row: ElemId, col: ElemId) -> Option<Value> {
        match self.c.cells.get(&cell_key(row, col))? {
            ValueOrContainer::Value(v) => Value::from_loro(&v),
            ValueOrContainer::Container(_) => None,
        }
    }

    /// Every readable cell of a row and column that are there.
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
