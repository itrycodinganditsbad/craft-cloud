//! Timelines: `<key>.tracks` (a movable list of track IDs), `<key>.track` (a map from track ID to the track's
//! properties), `<key>.items` (a map from item ID to its track, start, length and properties) and `<key>.keys` (a map
//! from keyframe ID to its item, time and value). Items and keyframes are keyed, never positional, so two machines
//! adding keyframes to one item keep both, and an item moved to another track on one machine while trimmed on another
//! keeps both changes. Items read back by start (then ID); keyframes by time (then ID). An item whose track is gone,
//! or a keyframe whose item is gone, is never read back.

use std::collections::{BTreeSet, HashMap, HashSet};

use loro::{LoroDoc, LoroMap, LoroMovableList, StyleConfigMap, ValueOrContainer};

use crate::mapping::{Span, TimelineSource, props_of};
use crate::props::{Props, PropsRead};
use crate::writer::Writer;
use crate::{ElemId, KitError, Value};

const TRACK: &str = "$track";
const START: &str = "$start";
const LEN: &str = "$len";
const OWNER: &str = "$item";
const AT: &str = "$at";
const VALUE: &str = "$value";

pub(crate) fn tracks_name(key: &str) -> String {
    format!("{key}.tracks")
}
pub(crate) fn track_props_name(key: &str) -> String {
    format!("{key}.track")
}
pub(crate) fn items_name(key: &str) -> String {
    format!("{key}.items")
}
pub(crate) fn keys_name(key: &str) -> String {
    format!("{key}.keys")
}

struct Containers {
    tracks: LoroMovableList,
    track_props: LoroMap,
    items: LoroMap,
    keys: LoroMap,
}

fn containers(doc: &LoroDoc, key: &str) -> Containers {
    Containers {
        tracks: doc.get_movable_list(tracks_name(key)),
        track_props: doc.get_map(track_props_name(key)),
        items: doc.get_map(items_name(key)),
        keys: doc.get_map(keys_name(key)),
    }
}

fn child_map(w: &mut Writer<'_>, parent: &LoroMap, key: &str) -> Result<LoroMap, KitError> {
    if let Some(ValueOrContainer::Container(loro::Container::Map(map))) = parent.get(key) {
        return Ok(map);
    }
    w.spend(key.len())?;
    // Mergeable: two machines making the same key's map at once share one.
    Ok(parent.ensure_mergeable_map(key)?)
}

fn existing_map(parent: &LoroMap, key: &str) -> Option<LoroMap> {
    match parent.get(key)? {
        ValueOrContainer::Container(loro::Container::Map(map)) => Some(map),
        _ => None,
    }
}

/// Every item of a timeline: its track and whether it was skipped.
fn all_items(src: &dyn TimelineSource) -> HashMap<ElemId, ElemId> {
    let mut out = HashMap::new();
    for track in src.tracks() {
        for item in src.items(track) {
            out.insert(item, track);
        }
    }
    out
}

/// Writes what changed from `old` to `new` (everything, when `old` is `None`) into the timeline `key`.
pub(crate) fn write(
    w: &mut Writer<'_>,
    key: &str,
    old: Option<&dyn TimelineSource>,
    new: &dyn TimelineSource,
    styles: &mut StyleConfigMap,
) -> Result<(), KitError> {
    if let (Some(old), Some(k)) = (old, new.key())
        && old.key() == Some(k)
    {
        return Ok(());
    }
    let c = containers(w.doc, key);
    let tracks = new.tracks();
    let removed_tracks = crate::list::sync(w, &c.tracks, &tracks)?;
    for track in removed_tracks {
        w.delete(&c.track_props, &track.hex())?;
    }
    let new_items = all_items(new);
    let old_items = old.map(all_items).unwrap_or_default();
    for track in &tracks {
        let same_track = old.is_some_and(|o| new.track_key(*track).is_some() && o.track_key(*track) == new.track_key(*track));
        if same_track {
            continue;
        }
        let map = child_map(w, &c.track_props, &track.hex())?;
        let mut props = Props::new(map, w, styles);
        new.write_track(*track, &mut props)?;
        props.finish()?;
        for item in new.items(*track) {
            let same_item =
                old.is_some_and(|o| new.item_key(item).is_some() && o.item_key(item) == new.item_key(item)) && old_items.get(&item) == Some(track);
            if same_item {
                continue;
            }
            write_item(w, &c, item, *track, old, new, styles)?;
        }
    }
    // Items gone, with their keyframes.
    for (item, _) in old_items.iter().filter(|(i, _)| !new_items.contains_key(i)) {
        if let Some(o) = old {
            for (k, _, _) in o.keyframes(*item) {
                w.delete(&c.keys, &k.hex())?;
            }
        }
        w.delete(&c.items, &item.hex())?;
    }
    Ok(())
}

fn write_item(
    w: &mut Writer<'_>,
    c: &Containers,
    item: ElemId,
    track: ElemId,
    old: Option<&dyn TimelineSource>,
    new: &dyn TimelineSource,
    styles: &mut StyleConfigMap,
) -> Result<(), KitError> {
    let Some(Span { start, len }) = new.span(item) else {
        return Err(KitError::Invalid(format!("the item {item} has no span")));
    };
    let map = child_map(w, &c.items, &item.hex())?;
    w.set(&map, TRACK, &Value::Bytes(track.0.to_le_bytes().to_vec()))?;
    w.set(&map, START, &Value::I64(start))?;
    w.set(&map, LEN, &Value::I64(len))?;
    let mut props = Props::new(map, w, styles);
    new.write_item(item, &mut props)?;
    props.finish()?;
    // Keyframes: by ID; only the ones that changed.
    let before: HashMap<ElemId, (i64, Value)> =
        old.map(|o| o.keyframes(item)).unwrap_or_default().into_iter().map(|(k, at, v)| (k, (at, v))).collect();
    let mut now = HashSet::new();
    for (k, at, value) in new.keyframes(item) {
        now.insert(k);
        if old.is_some() && before.get(&k) == Some(&(at, value.clone())) {
            continue;
        }
        let map = child_map(w, &c.keys, &k.hex())?;
        w.set(&map, OWNER, &Value::Bytes(item.0.to_le_bytes().to_vec()))?;
        w.set(&map, AT, &Value::I64(at))?;
        w.set(&map, VALUE, &value)?;
    }
    for k in before.keys().filter(|k| !now.contains(k)) {
        w.delete(&c.keys, &k.hex())?;
    }
    Ok(())
}

fn id_value(map: &LoroMap, key: &str) -> Option<ElemId> {
    match map.get(key)? {
        ValueOrContainer::Value(loro::LoroValue::Binary(b)) => <[u8; 16]>::try_from(b.as_slice()).ok().map(|a| ElemId(u128::from_le_bytes(a))),
        _ => None,
    }
}

fn i64_value(map: &LoroMap, key: &str) -> Option<i64> {
    match map.get(key)? {
        ValueOrContainer::Value(loro::LoroValue::I64(i)) => Some(i),
        _ => None,
    }
}

/// What a remote change touched of a timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimelineTouch {
    /// Everything (read it whole).
    pub all: bool,
    /// The track list changed.
    pub tracks: bool,
    /// Tracks whose properties changed.
    pub track_props: BTreeSet<ElemId>,
    /// Items written or removed (their keyframes too).
    pub items: BTreeSet<ElemId>,
    /// Keyframes written or removed.
    pub keyframes: BTreeSet<ElemId>,
}

impl TimelineTouch {
    pub(crate) fn whole() -> Self {
        Self { all: true, ..Self::default() }
    }
}

/// One item as stored.
#[derive(Debug, Clone)]
pub struct ItemRead {
    pub id: ElemId,
    pub track: ElemId,
    pub span: Span,
    pub props: PropsRead,
}

/// A timeline as stored, after a remote change.
pub struct TimelineView<'a> {
    c: Containers,
    touch: TimelineTouch,
    _doc: std::marker::PhantomData<&'a LoroDoc>,
}

impl<'a> TimelineView<'a> {
    pub(crate) fn new(doc: &'a LoroDoc, key: &str, touch: TimelineTouch) -> Self {
        Self { c: containers(doc, key), touch, _doc: std::marker::PhantomData }
    }

    pub fn touch(&self) -> &TimelineTouch {
        &self.touch
    }

    /// The tracks, in order.
    pub fn tracks(&self) -> Vec<ElemId> {
        crate::list::ids(&self.c.tracks)
    }

    /// A track's properties.
    pub fn track_props(&self, track: ElemId) -> Option<PropsRead> {
        existing_map(&self.c.track_props, &track.hex()).map(props_of)
    }

    /// An item, if it is on a track that is there.
    pub fn item(&self, id: ElemId) -> Option<ItemRead> {
        let map = existing_map(&self.c.items, &id.hex())?;
        let track = id_value(&map, TRACK)?;
        if !self.tracks().contains(&track) {
            return None;
        }
        let span = Span { start: i64_value(&map, START)?, len: i64_value(&map, LEN)? };
        Some(ItemRead { id, track, span, props: props_of(map) })
    }

    /// Every item on a track that is there, by track, each by start then ID.
    pub fn items(&self) -> HashMap<ElemId, Vec<ItemRead>> {
        let tracks: HashSet<ElemId> = self.tracks().into_iter().collect();
        let mut out: HashMap<ElemId, Vec<ItemRead>> = HashMap::new();
        let mut ids = Vec::new();
        self.c.items.for_each(|k, _| ids.extend(ElemId::from_hex(k)));
        for id in ids {
            if let Some(item) = self.item(id)
                && tracks.contains(&item.track)
            {
                out.entry(item.track).or_default().push(item);
            }
        }
        for list in out.values_mut() {
            list.sort_by_key(|i| (i.span.start, i.id));
        }
        out
    }

    /// An item's keyframes: ID, time and value, by time then ID.
    pub fn keyframes(&self, item: ElemId) -> Vec<(ElemId, i64, Value)> {
        let mut out = Vec::new();
        let mut ids = Vec::new();
        self.c.keys.for_each(|k, _| ids.extend(ElemId::from_hex(k)));
        for id in ids {
            let Some(map) = existing_map(&self.c.keys, &id.hex()) else { continue };
            if id_value(&map, OWNER) != Some(item) {
                continue;
            }
            let (Some(at), Some(ValueOrContainer::Value(v))) = (i64_value(&map, AT), map.get(VALUE)) else { continue };
            if let Some(v) = Value::from_loro(&v) {
                out.push((id, at, v));
            }
        }
        out.sort_by_key(|(id, at, _)| (*at, *id));
        out
    }

    /// Every keyframe, by item.
    pub fn all_keyframes(&self) -> HashMap<ElemId, Vec<(ElemId, i64, Value)>> {
        let mut out: HashMap<ElemId, Vec<(ElemId, i64, Value)>> = HashMap::new();
        let mut ids = Vec::new();
        self.c.keys.for_each(|k, _| ids.extend(ElemId::from_hex(k)));
        for id in ids {
            let Some(map) = existing_map(&self.c.keys, &id.hex()) else { continue };
            let (Some(owner), Some(at), Some(ValueOrContainer::Value(v))) = (id_value(&map, OWNER), i64_value(&map, AT), map.get(VALUE)) else {
                continue;
            };
            if let Some(v) = Value::from_loro(&v) {
                out.entry(owner).or_default().push((id, at, v));
            }
        }
        for list in out.values_mut() {
            list.sort_by_key(|(id, at, _)| (*at, *id));
        }
        out
    }
}
