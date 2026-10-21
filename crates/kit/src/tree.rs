//! Trees: one `LoroTree` per part (fractional index on, so children keep their order), each node's meta map holding
//! the app's properties and the element's ID under `$id`. Moves and reparenting merge without duplicates, and a move
//! that would make a cycle with a concurrent one is dropped by Loro.
//!
//! The diff walks the new model from its roots, skipping every subtree whose [`TreeSource::subtree_key`] is the same in
//! the old one; under each parent it visits, it creates what is missing, moves only what is out of order (the longest
//! run in order stays), and deletes, at the end, what left and went nowhere else.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use loro::{LoroDoc, LoroTree, StyleConfigMap, TextDelta, TreeID, TreeParentId, ValueOrContainer};

use crate::mapping::{TextSource, TreeSource, props_of};
use crate::props::{Props, PropsRead};
use crate::text::RichText;
use crate::writer::Writer;
use crate::{ElemId, KitError};

/// The meta key that holds a node's element ID.
const ID_KEY: &str = "$id";
const NODE_WEIGHT: usize = 24;

/// Which Loro node is which element, for one tree.
#[derive(Debug, Clone, Default)]
pub struct TreeIndex {
    pub(crate) tid_of: HashMap<ElemId, TreeID>,
    pub(crate) elem_of: HashMap<TreeID, ElemId>,
}

impl TreeIndex {
    /// Every live node of `tree` with an ID (the first node with an ID wins; a second is never read).
    pub(crate) fn build(tree: &LoroTree) -> Self {
        let mut index = Self::default();
        let mut nodes = tree.nodes();
        nodes.sort();
        for tid in nodes {
            if tree.is_node_deleted(&tid).unwrap_or(true) {
                continue;
            }
            if let Some(id) = node_id(tree, tid)
                && !index.tid_of.contains_key(&id)
            {
                index.tid_of.insert(id, tid);
                index.elem_of.insert(tid, id);
            }
        }
        index
    }

    fn insert(&mut self, id: ElemId, tid: TreeID) {
        self.tid_of.insert(id, tid);
        self.elem_of.insert(tid, id);
    }

    pub(crate) fn len(&self) -> usize {
        self.tid_of.len()
    }
}

pub(crate) fn node_id(tree: &LoroTree, tid: TreeID) -> Option<ElemId> {
    let meta = tree.get_meta(tid).ok()?;
    match meta.get(ID_KEY)? {
        ValueOrContainer::Value(v) => ElemId::from_loro(&v),
        ValueOrContainer::Container(_) => None,
    }
}

/// Writes what changed from `old` to `new` (everything, when `old` is `None`) into the tree `key`.
pub(crate) fn write(
    w: &mut Writer<'_>,
    key: &str,
    old: Option<&dyn TreeSource>,
    new: &dyn TreeSource,
    index: &mut TreeIndex,
    styles: &mut StyleConfigMap,
) -> Result<(), KitError> {
    if let (Some(old), Some(k)) = (old, new.key())
        && old.key() == Some(k)
    {
        return Ok(());
    }
    let tree = w.doc.get_tree(key);
    tree.enable_fractional_index(0);
    let mut visited: HashSet<ElemId> = HashSet::new();
    let mut leaving: Vec<TreeID> = Vec::new();
    let mut deleted_any = false;
    let mut skipped: Vec<TreeID> = Vec::new();
    // (parent, its children in the new model), depth first.
    let mut stack: Vec<(TreeParentId, Vec<ElemId>)> = vec![(TreeParentId::Root, new.roots())];
    while let Some((parent, desired)) = stack.pop() {
        let mut created: HashSet<ElemId> = HashSet::new();
        for id in &desired {
            if !visited.insert(*id) {
                return Err(KitError::Invalid(format!("the element {id} is in the tree twice")));
            }
            if !index.tid_of.contains_key(id) {
                w.spend(NODE_WEIGHT)?;
                let tid = tree.create(parent)?;
                tree.get_meta(tid)?.insert(ID_KEY, id.to_loro())?;
                index.insert(*id, tid);
                created.insert(*id);
            }
        }
        order(w, &tree, parent, &desired, index, &mut leaving)?;
        for id in desired.iter().rev() {
            let Some(tid) = index.tid_of.get(id).copied() else { continue };
            let fresh = created.contains(id);
            let unchanged = !fresh && old.is_some_and(|old| old.subtree_key(*id).is_some() && old.subtree_key(*id) == new.subtree_key(*id));
            if unchanged {
                skipped.push(tid);
                continue;
            }
            let mut props = Props::new(tree.get_meta(tid)?, w, styles);
            new.write(*id, &mut props)?;
            props.finish()?;
            stack.push((TreeParentId::Node(tid), new.children(*id)));
        }
    }
    if !leaving.is_empty() {
        // Nothing under an unchanged subtree leaves (it would have changed): kept even if the app says otherwise.
        for tid in skipped {
            mark_subtree(&tree, tid, index, &mut visited);
        }
    }
    for tid in leaving {
        let Some(id) = index.elem_of.get(&tid).copied() else { continue };
        if visited.contains(&id) || tree.is_node_deleted(&tid).unwrap_or(true) {
            continue;
        }
        w.spend(NODE_WEIGHT)?;
        tree.delete(tid)?;
        deleted_any = true;
    }
    if deleted_any {
        index.tid_of.retain(|_, tid| !tree.is_node_deleted(tid).unwrap_or(true));
        index.elem_of.retain(|tid, _| !tree.is_node_deleted(tid).unwrap_or(true));
    }
    Ok(())
}

/// Marks everything under `tid` as placed (an unchanged subtree).
fn mark_subtree(tree: &LoroTree, tid: TreeID, index: &TreeIndex, visited: &mut HashSet<ElemId>) {
    let mut stack = vec![tid];
    while let Some(t) = stack.pop() {
        for child in tree.children(TreeParentId::Node(t)).unwrap_or_default() {
            if let Some(id) = index.elem_of.get(&child) {
                visited.insert(*id);
            }
            stack.push(child);
        }
    }
}

/// Puts `desired` (all existing nodes) under `parent` in that order, moving as few as it can; the children there
/// that aren't wanted go to `leaving`.
fn order(
    w: &mut Writer<'_>,
    tree: &LoroTree,
    parent: TreeParentId,
    desired: &[ElemId],
    index: &TreeIndex,
    leaving: &mut Vec<TreeID>,
) -> Result<(), KitError> {
    let current = tree.children(parent).unwrap_or_default();
    let rank: HashMap<ElemId, usize> = desired.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let mut here: Vec<usize> = Vec::new();
    for tid in &current {
        match index.elem_of.get(tid).and_then(|id| rank.get(id)) {
            Some(r) => here.push(*r),
            None => leaving.push(*tid),
        }
    }
    let in_order = here.windows(2).all(|p| p.first() < p.get(1)) && here.len() == desired.len();
    if in_order {
        return Ok(());
    }
    let steady: HashSet<usize> = crate::list::longest_increasing(&here).into_iter().collect();
    let mut prev: Option<TreeID> = None;
    for (r, id) in desired.iter().enumerate() {
        let Some(tid) = index.tid_of.get(id).copied() else { continue };
        if !steady.contains(&r) {
            w.spend(NODE_WEIGHT)?;
            match prev {
                Some(p) => tree.mov_after(tid, p)?,
                None => tree.mov_to(tid, parent, 0)?,
            }
        }
        prev = Some(tid);
    }
    Ok(())
}

/// What a remote change touched of a tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeTouch {
    /// Everything (read it whole).
    pub all: bool,
    /// Nodes were created, moved or deleted: read the structure again.
    pub structure: bool,
    /// The nodes whose properties, stories or tables changed (and the ones created).
    pub nodes: BTreeSet<ElemId>,
    /// What changed inside nodes' child containers: the node and the path of keys under it (`text` for the story
    /// `text`; `table/cells/<row>:<col>/text` for a story in a cell of the table `table`).
    pub stories: BTreeSet<(ElemId, String)>,
}

impl TreeTouch {
    pub(crate) fn whole() -> Self {
        Self { all: true, structure: true, ..Self::default() }
    }
}

/// Loro's text deltas of the stories nested in a tree's nodes, by node and story key, in the order they came.
pub(crate) type NodeDeltas = BTreeMap<(ElemId, String), Vec<Vec<TextDelta>>>;

/// A tree as stored, after a remote change.
pub struct TreeView<'a> {
    tree: LoroTree,
    index: &'a TreeIndex,
    touch: TreeTouch,
    deltas: Option<&'a NodeDeltas>,
}

impl<'a> TreeView<'a> {
    pub(crate) fn new(doc: &'a LoroDoc, key: &str, index: &'a TreeIndex, touch: TreeTouch, deltas: Option<&'a NodeDeltas>) -> Self {
        Self { tree: doc.get_tree(key), index, touch, deltas }
    }

    /// An element's story `key` as stored (`None`: it has none).
    pub fn text(&self, id: ElemId, key: &str) -> Option<RichText> {
        self.props(id)?.text(key)
    }

    /// An element's story `key`, patched from `old` (the app's text of it before the change: only what changed is
    /// touched) when the remote change's deltas fit it, else read whole. `None`: it has none.
    pub fn text_patched(&self, id: ElemId, key: &str, old: Option<&dyn TextSource>) -> Option<RichText> {
        let deltas = self.deltas.and_then(|d| d.get(&(id, key.to_owned())));
        self.props(id)?.text_patched(key, old, deltas)
    }

    pub fn touch(&self) -> &TreeTouch {
        &self.touch
    }

    fn known(&self, tids: Vec<TreeID>) -> Vec<ElemId> {
        tids.into_iter().filter_map(|t| self.index.elem_of.get(&t).copied()).collect()
    }

    /// The top-level elements, in order.
    pub fn roots(&self) -> Vec<ElemId> {
        self.known(self.tree.children(TreeParentId::Root).unwrap_or_default())
    }

    /// An element's children, in order.
    pub fn children(&self, id: ElemId) -> Vec<ElemId> {
        match self.index.tid_of.get(&id) {
            Some(tid) => self.known(self.tree.children(TreeParentId::Node(*tid)).unwrap_or_default()),
            None => Vec::new(),
        }
    }

    /// An element's parent (`None` for a top-level one or an unknown ID).
    pub fn parent(&self, id: ElemId) -> Option<ElemId> {
        match self.tree.parent(*self.index.tid_of.get(&id)?)? {
            TreeParentId::Node(p) => self.index.elem_of.get(&p).copied(),
            _ => None,
        }
    }

    pub fn contains(&self, id: ElemId) -> bool {
        self.index.tid_of.contains_key(&id)
    }

    /// An element's properties.
    pub fn props(&self, id: ElemId) -> Option<PropsRead> {
        let tid = self.index.tid_of.get(&id)?;
        self.tree.get_meta(*tid).ok().map(props_of)
    }

    /// How many elements the tree has.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.len() == 0
    }
}
