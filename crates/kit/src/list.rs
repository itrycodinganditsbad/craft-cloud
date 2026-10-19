//! Keeping a `LoroMovableList` of element IDs in a given order with few operations: what isn't wanted (or is there
//! twice, or isn't an ID) is deleted, the longest run already in order stays, and only the rest moves or is inserted,
//! so concurrent reorders on two machines merge rather than fight.

use std::collections::{HashMap, HashSet};

use loro::{LoroMovableList, ValueOrContainer};

use crate::writer::Writer;
use crate::{ElemId, KitError};

const ID_WEIGHT: usize = 16;

/// The list's IDs as stored (anything else is `None`).
pub(crate) fn read(list: &LoroMovableList) -> Vec<Option<ElemId>> {
    (0..list.len())
        .map(|i| match list.get(i) {
            Some(ValueOrContainer::Value(v)) => ElemId::from_loro(&v),
            _ => None,
        })
        .collect()
}

/// The list's IDs, each once (first place wins), skipping anything that isn't one.
pub(crate) fn ids(list: &LoroMovableList) -> Vec<ElemId> {
    let mut seen = HashSet::new();
    read(list).into_iter().flatten().filter(|id| seen.insert(*id)).collect()
}

/// Makes `list` hold exactly `desired` (IDs, each once), in order. Returns the IDs it removed.
pub(crate) fn sync(w: &mut Writer<'_>, list: &LoroMovableList, desired: &[ElemId]) -> Result<Vec<ElemId>, KitError> {
    let want: HashSet<ElemId> = desired.iter().copied().collect();
    let stored = read(list);
    // Which places to keep: a wanted ID's first place.
    let mut seen = HashSet::new();
    let keep: Vec<bool> = stored.iter().map(|id| id.is_some_and(|id| want.contains(&id) && seen.insert(id))).collect();
    let mut removed = Vec::new();
    for (i, kept) in keep.iter().enumerate().rev() {
        if !kept {
            if let Some(Some(id)) = stored.get(i)
                && !want.contains(id)
            {
                removed.push(*id);
            }
            w.spend(ID_WEIGHT)?;
            list.delete(i, 1)?;
        }
    }
    let mut cur: Vec<ElemId> = stored.iter().zip(&keep).filter(|(_, k)| **k).filter_map(|(id, _)| *id).collect();
    if cur.as_slice() == desired {
        return Ok(removed);
    }
    // The longest run of what is there already in the wanted order stays put.
    let rank: HashMap<ElemId, usize> = desired.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let ranks: Vec<usize> = cur.iter().filter_map(|id| rank.get(id).copied()).collect();
    let steady: HashSet<ElemId> = longest_increasing(&ranks).into_iter().filter_map(|i| desired.get(i).copied()).collect();
    let mut prev: Option<ElemId> = None;
    for id in desired {
        if !steady.contains(id) {
            let to = match prev {
                Some(p) => cur.iter().position(|c| *c == p).map_or(0, |i| i + 1),
                None => 0,
            };
            w.spend(ID_WEIGHT)?;
            match cur.iter().position(|c| c == id) {
                Some(from) => {
                    cur.remove(from);
                    let to = if from < to { to - 1 } else { to };
                    list.mov(from, to)?;
                    cur.insert(to, *id);
                }
                None => {
                    list.insert(to, id.to_loro())?;
                    cur.insert(to, *id);
                }
            }
        }
        prev = Some(*id);
    }
    Ok(removed)
}

/// The values of the longest strictly increasing subsequence of `seq`.
pub(crate) fn longest_increasing(seq: &[usize]) -> Vec<usize> {
    // Patience sorting: `tails[k]` is the index (into `seq`) ending the best run of length k + 1.
    let mut tails: Vec<usize> = Vec::new();
    let mut back: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, v) in seq.iter().enumerate() {
        let k = tails.partition_point(|&t| seq.get(t).is_some_and(|tv| tv < v));
        if k > 0
            && let Some(slot) = back.get_mut(i)
        {
            *slot = tails.get(k - 1).copied();
        }
        if k == tails.len() {
            tails.push(i);
        } else if let Some(slot) = tails.get_mut(k) {
            *slot = i;
        }
    }
    let mut out = Vec::new();
    let mut at = tails.last().copied();
    while let Some(i) = at {
        if let Some(v) = seq.get(i) {
            out.push(*v);
        }
        at = back.get(i).copied().flatten();
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use loro::LoroDoc;

    fn ids(n: u128) -> Vec<ElemId> {
        (0..n).map(ElemId).collect()
    }

    #[test]
    fn a_list_is_brought_to_order_with_few_moves() {
        assert_eq!(longest_increasing(&[3, 0, 1, 2]), vec![0, 1, 2]);
        assert_eq!(longest_increasing(&[]), Vec::<usize>::new());
        let doc = LoroDoc::new();
        let list = doc.get_movable_list("l");
        let all = ids(6);
        for (from, to) in [
            (vec![], all.clone()),
            (all.clone(), vec![all[1], all[2], all[3], all[4], all[5], all[0]]),
            (all.clone(), vec![all[5], all[0], all[1]]),
            (all.clone(), vec![all[2], all[0], all[4], all[1]]),
            (all.clone(), all.iter().rev().copied().collect()),
        ] {
            let mut w = Writer::new(&doc, crate::writer::LOCAL_ORIGIN);
            sync(&mut w, &list, &from).unwrap();
            w.finish().unwrap();
            assert_eq!(super::ids(&list), from);
            let before = doc.oplog_vv();
            let mut w = Writer::new(&doc, crate::writer::LOCAL_ORIGIN);
            sync(&mut w, &list, &to).unwrap();
            w.finish().unwrap();
            assert_eq!(super::ids(&list), to, "from {from:?}");
            let ops: i32 = doc.oplog_vv().iter().map(|(p, c)| c - before.get(p).copied().unwrap_or(0)).sum();
            if from.len() == 6 && to.len() == 6 && to.first() == Some(&all[1]) {
                assert_eq!(ops, 1, "one move: the first to the end");
            }
        }
    }
}
