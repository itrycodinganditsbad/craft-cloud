//! A model element's properties as a Loro map: what the app's codec writes ([`Props`], only what changed) and reads
//! back ([`PropsRead`]). Keys starting with `$` are the kit's own.
//!
//! Besides values, an element's map can hold a rich-text story ([`Props::text`]) and a table ([`Props::table`]),
//! each under a key the codec names: a mergeable child container of the map, so two machines making it at once make
//! one, and their edits in it merge (two people typing in one shape's text merge character by character).

use loro::{Container, LoroMap, StyleConfigMap, ValueOrContainer};

use crate::mapping::{TableSource, TextSource};
use crate::table::TableView;
use crate::text::{RichText, Story};
use crate::writer::Writer;
use crate::{KitError, Value};

/// Checks an app's property key: 1 to 128 bytes, not starting with `$` (the kit's) or `¶` (paragraph marks).
pub(crate) fn check_key(key: &str) -> Result<(), KitError> {
    if key.is_empty() || key.len() > 128 || key.starts_with('$') || key.starts_with('¶') {
        return Err(KitError::Invalid(format!("{key:?} isn't a property key (1-128 bytes, not starting with '$' or '¶')")));
    }
    Ok(())
}

/// The write side of one element's properties: set what the element has; what it no longer has goes when the writer
/// is done. A value equal to the stored one writes nothing.
pub struct Props<'a, 'd> {
    pub(crate) map: LoroMap,
    pub(crate) w: &'a mut Writer<'d>,
    pub(crate) written: Vec<String>,
    pub(crate) styles: &'a mut StyleConfigMap,
}

impl<'a, 'd> Props<'a, 'd> {
    pub(crate) fn new(map: LoroMap, w: &'a mut Writer<'d>, styles: &'a mut StyleConfigMap) -> Self {
        Self { map, w, written: Vec::new(), styles }
    }

    /// Sets the property `key`.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) -> Result<(), KitError> {
        check_key(key)?;
        let value = value.into();
        self.w.set(&self.map, key, &value)?;
        self.written.push(key.to_owned());
        Ok(())
    }

    /// Sets the rich-text story `key` (a shape's text, a cell's): only what differs from what it holds is written, as
    /// for a root story (character attributes as marks, paragraph attributes on the breaks).
    pub fn text(&mut self, key: &str, source: &dyn TextSource) -> Result<(), KitError> {
        check_key(key)?;
        let story = match Story::existing(&self.map, key) {
            Some(story) => story,
            None => {
                self.w.spend(crate::writer::OP_OVERHEAD)?;
                Story::nested(&self.map, key)?
            }
        };
        crate::text::write_story(self.w, &story, None, source, self.styles)?;
        self.written.push(key.to_owned());
        Ok(())
    }

    /// Sets the table `key` (a table shape's rows, columns and cells): rows and columns are movable lists of IDs, so
    /// rows inserted on two machines at once are both kept; cells are as [`TableSource`] says (values, or property
    /// maps with stories of their own).
    pub fn table(&mut self, key: &str, source: &dyn TableSource) -> Result<(), KitError> {
        check_key(key)?;
        let map = match self.map.get(key) {
            Some(ValueOrContainer::Container(Container::Map(map))) => map,
            _ => {
                self.w.spend(crate::writer::OP_OVERHEAD)?;
                self.map.ensure_mergeable_map(key)?
            }
        };
        crate::table::write_nested(self.w, &map, source, self.styles)?;
        self.written.push(key.to_owned());
        Ok(())
    }

    /// Deletes the properties not set (the kit's own keys stay, but a story's `$last.<key>` goes with it).
    pub(crate) fn finish(self) -> Result<(), KitError> {
        let mut stale = Vec::new();
        self.map.for_each(|key, _| {
            let gone = match key.strip_prefix("$last.") {
                Some(story) => !self.written.iter().any(|w| w == story),
                None => !key.starts_with('$') && !self.written.iter().any(|w| w == key),
            };
            if gone {
                stale.push(key.to_owned());
            }
        });
        for key in stale {
            self.w.delete(&self.map, &key)?;
        }
        Ok(())
    }
}

/// One element's properties as stored.
#[derive(Debug, Clone)]
pub struct PropsRead {
    pub(crate) map: LoroMap,
}

impl PropsRead {
    /// The property `key`, if it holds a value an app writes.
    pub fn get(&self, key: &str) -> Option<Value> {
        match self.map.get(key)? {
            ValueOrContainer::Value(v) => Value::from_loro(&v),
            ValueOrContainer::Container(_) => None,
        }
    }

    pub fn str(&self, key: &str) -> Option<String> {
        self.get(key).and_then(|v| v.as_str().map(str::to_owned))
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        self.get(key).and_then(|v| v.as_i64())
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.get(key).and_then(|v| v.as_f64())
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(|v| v.as_bool())
    }

    pub fn bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.get(key).and_then(|v| v.as_bytes().map(<[u8]>::to_vec))
    }

    pub fn chunk(&self, key: &str) -> Option<crate::ChunkRef> {
        self.get(key).and_then(|v| v.as_chunk())
    }

    /// The story `key` as stored (`None`: there is none).
    pub fn text(&self, key: &str) -> Option<RichText> {
        Story::existing(&self.map, key).map(|s| crate::text::read_story(&s))
    }

    /// The story `key`, patched from `old` (the app's text before the change) with the `deltas` a remote change
    /// brought, else read whole.
    pub(crate) fn text_patched(&self, key: &str, old: Option<&dyn TextSource>, deltas: Option<&Vec<Vec<loro::TextDelta>>>) -> Option<RichText> {
        let story = Story::existing(&self.map, key)?;
        if let (Some(old), Some(deltas)) = (old, deltas)
            && let Some(patched) = crate::text::patch_story(&story, old, deltas)
        {
            return Some(patched);
        }
        Some(crate::text::read_story(&story))
    }

    /// The table `key` as stored (`None`: there is none). Read it whole: a nested table's touch is always whole.
    pub fn table(&self, key: &str) -> Option<TableView<'static>> {
        match self.map.get(key)? {
            ValueOrContainer::Container(Container::Map(map)) => Some(TableView::nested(&map)),
            _ => None,
        }
    }

    /// Every app property that holds a value, sorted by key (stories and tables aren't values).
    pub fn all(&self) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        self.map.for_each(|key, value| {
            if key.starts_with('$') {
                return;
            }
            if let ValueOrContainer::Value(v) = value
                && let Some(v) = Value::from_loro(&v)
            {
                out.push((key.to_owned(), v));
            }
        });
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}
