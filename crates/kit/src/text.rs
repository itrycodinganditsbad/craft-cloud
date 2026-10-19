//! Rich text: one `LoroText` per part. Character attributes are marks (expanding after, as bold does when one types
//! on); paragraph attributes are marks named `¶<key>` on each paragraph's `\n` (never expanding), and the last
//! paragraph's, which has no `\n`, sit in the root map `<key>.last`.
//!
//! The diff takes the common prefix and suffix of the old and new text (so a keystroke in a 1 MB story touches one
//! spot), replaces what is between (in pieces, so a 5 MB paste splits into commits under the cap), then compares the
//! attributes run by run: the old ones moved past the edit against the new ones, and in the inserted text what Loro
//! actually gave it (what it inherited by expanding) against what the app wants. Only differences are marked.

use std::borrow::Cow;
use std::ops::Range;

use loro::cursor::PosType;
use loro::{ExpandType, LoroDoc, LoroText, StyleConfig, StyleConfigMap, TextDelta, ValueOrContainer};

use crate::mapping::{Attrs, TextSource};
use crate::writer::Writer;
use crate::{KitError, Value};

/// A character run: a byte range with its attributes.
type Run = (Range<usize>, Attrs);

/// The prefix of paragraph marks.
const PARA: char = '¶';

pub(crate) fn last_name(key: &str) -> String {
    format!("{key}.last")
}

/// A story as plain data: text, character runs and paragraph attributes. It is a [`TextSource`] too.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RichText {
    pub text: String,
    /// Byte ranges with their character attributes (in order, not overlapping; none where no run is).
    pub runs: Vec<(Range<usize>, Attrs)>,
    /// One per paragraph (one more than the text has `\n`s).
    pub paragraphs: Vec<Attrs>,
}

impl RichText {
    /// Plain text: no attributes.
    pub fn plain(text: &str) -> Self {
        let paragraphs = vec![Attrs::new(); text.matches('\n').count() + 1];
        Self { text: text.to_owned(), runs: Vec::new(), paragraphs }
    }

    /// The character attributes at byte `at`.
    pub fn attrs_at(&self, at: usize) -> Attrs {
        self.runs.iter().find(|(r, _)| r.contains(&at)).map(|(_, a)| a.clone()).unwrap_or_default()
    }
}

impl TextSource for RichText {
    fn text(&self) -> &str {
        &self.text
    }

    fn runs(&self) -> Cow<'_, [(Range<usize>, Attrs)]> {
        Cow::Borrowed(&self.runs)
    }

    fn paragraphs(&self) -> Cow<'_, [Attrs]> {
        Cow::Borrowed(&self.paragraphs)
    }
}

/// The style config the kit gives every key: character marks expand after, paragraph marks never.
pub(crate) fn configure(doc: &LoroDoc, styles: &mut StyleConfigMap, key: &str) {
    if key.starts_with(PARA) && styles.get(&key.into()).is_none() {
        styles.insert(key.into(), StyleConfig { expand: ExpandType::None });
        doc.config_text_style(styles.clone());
    }
}

/// The longest common prefix of `a` and `b`, in bytes, on a character boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let max = x.len().min(y.len());
    let mut i = 0;
    // Whole blocks first (compared as slices: fast), then byte by byte.
    const BLOCK: usize = 4096;
    while i + BLOCK <= max && x.get(i..i + BLOCK) == y.get(i..i + BLOCK) {
        i += BLOCK;
    }
    while i < max && x.get(i) == y.get(i) {
        i += 1;
    }
    while i > 0 && !a.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The longest common suffix of `a` and `b` of at most `max` bytes, on a character boundary.
fn common_suffix(a: &str, b: &str, max: usize) -> usize {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    let mut n = 0;
    const BLOCK: usize = 4096;
    while n + BLOCK <= max && x.get(x.len() - n - BLOCK..x.len() - n) == y.get(y.len() - n - BLOCK..y.len() - n) {
        n += BLOCK;
    }
    while n < max && x.get(x.len() - n - 1) == y.get(y.len() - n - 1) {
        n += 1;
    }
    while n > 0 && !a.is_char_boundary(a.len() - n) {
        n -= 1;
    }
    n
}

/// Checks runs: in order, not overlapping, inside the text, on character boundaries, not empty.
fn check_runs(text: &str, runs: &[(Range<usize>, Attrs)]) -> Result<(), KitError> {
    let mut at = 0;
    for (r, attrs) in runs {
        if r.start < at || r.end <= r.start || r.end > text.len() || !text.is_char_boundary(r.start) || !text.is_char_boundary(r.end) {
            return Err(KitError::Invalid(format!("the run {r:?} isn't in order, inside the text and on character boundaries")));
        }
        for (k, v) in attrs {
            crate::props::check_key(k)?;
            v.check()?;
        }
        at = r.end;
    }
    Ok(())
}

/// Loro's attributes as the kit's, character ones and paragraph ones apart.
fn split_attrs<'a>(attributes: impl IntoIterator<Item = (&'a String, &'a loro::LoroValue)>) -> (Attrs, Attrs) {
    let (mut chars, mut paras) = (Attrs::new(), Attrs::new());
    for (k, v) in attributes {
        let Some(v) = Value::from_loro(v) else { continue };
        if matches!(v, Value::Null) {
            continue;
        }
        match k.strip_prefix(PARA) {
            Some(k) => paras.insert(k.to_owned(), v),
            None => chars.insert(k.clone(), v),
        };
    }
    (chars, paras)
}

/// The text's character runs between bytes `lo` and `hi`, as Loro holds them.
fn stored_runs(text: &LoroText, lo: usize, hi: usize) -> Result<Vec<(Range<usize>, Attrs)>, KitError> {
    if hi <= lo {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut at = lo;
    for delta in text.slice_delta(lo, hi, PosType::Bytes)? {
        if let TextDelta::Insert { insert, attributes } = delta {
            let end = at + insert.len();
            let (chars, _) = split_attrs(attributes.iter().flatten());
            if !chars.is_empty() {
                out.push((at..end, chars));
            }
            at = end;
        }
    }
    Ok(out)
}

/// The old runs as they lie in the new text: the prefix's as they were, the suffix's moved by the edit, and in the
/// replaced middle what Loro holds there now.
fn moved_runs(
    old: &[(Range<usize>, Attrs)],
    p: usize,
    old_end: usize,
    new_end: usize,
    middle: Vec<(Range<usize>, Attrs)>,
) -> Vec<(Range<usize>, Attrs)> {
    let mut out = Vec::new();
    for (r, a) in old {
        if r.start < p {
            out.push((r.start..r.end.min(p), a.clone()));
        }
    }
    out.extend(middle);
    for (r, a) in old {
        if r.end > old_end {
            let start = r.start.max(old_end) - old_end + new_end;
            let end = r.end - old_end + new_end;
            out.push((start..end, a.clone()));
        }
    }
    out
}

/// Where `desired` and `current` (sorted runs) differ: each stretch with the keys to set (`Some`) or remove.
type Changes = Vec<(Range<usize>, Vec<(String, Option<Value>)>)>;

fn differences(desired: &[(Range<usize>, Attrs)], current: &[(Range<usize>, Attrs)]) -> Changes {
    let mut cuts: Vec<usize> = Vec::with_capacity((desired.len() + current.len()) * 2);
    for (r, _) in desired.iter().chain(current) {
        cuts.push(r.start);
        cuts.push(r.end);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let empty = Attrs::new();
    let at = |runs: &[(Range<usize>, Attrs)], i: &mut usize, x: usize| -> Attrs {
        while runs.get(*i).is_some_and(|(r, _)| r.end <= x) {
            *i += 1;
        }
        match runs.get(*i) {
            Some((r, a)) if r.start <= x => a.clone(),
            _ => empty.clone(),
        }
    };
    let (mut di, mut ci) = (0, 0);
    let mut out: Changes = Vec::new();
    for pair in cuts.windows(2) {
        let (Some(&x), Some(&y)) = (pair.first(), pair.get(1)) else { continue };
        let (want, have) = (at(desired, &mut di, x), at(current, &mut ci, x));
        if want == have {
            continue;
        }
        let mut ops = Vec::new();
        for (k, v) in &want {
            if have.get(k) != Some(v) {
                ops.push((k.clone(), Some(v.clone())));
            }
        }
        for k in have.keys() {
            if !want.contains_key(k) {
                ops.push((k.clone(), None));
            }
        }
        // A stretch right after one with the same changes grows it.
        match out.last_mut() {
            Some((r, last)) if r.end == x && *last == ops => r.end = y,
            _ => out.push((x..y, ops)),
        }
    }
    out
}

fn apply(w: &mut Writer<'_>, text: &LoroText, styles: &mut StyleConfigMap, changes: Changes, prefix: &str) -> Result<(), KitError> {
    for (range, ops) in changes {
        for (k, v) in ops {
            let key = format!("{prefix}{k}");
            configure(w.doc, styles, &key);
            match v {
                Some(v) => {
                    w.spend(16 + key.len() + v.weight())?;
                    text.mark_utf8(range.clone(), &key, v.to_loro())?;
                }
                None => {
                    w.spend(16 + key.len())?;
                    let start = text.convert_pos(range.start, PosType::Bytes, PosType::Unicode);
                    let end = text.convert_pos(range.end, PosType::Bytes, PosType::Unicode);
                    if let (Some(start), Some(end)) = (start, end) {
                        text.unmark(start..end, &key)?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The byte positions of the `\n`s of `text`.
fn breaks(text: &str) -> Vec<usize> {
    text.bytes().enumerate().filter(|(_, b)| *b == b'\n').map(|(i, _)| i).collect()
}

/// Writes what changed from `old` to `new` (everything, when `old` is `None`) into the text `key`.
pub(crate) fn write(
    w: &mut Writer<'_>,
    key: &str,
    old: Option<&dyn TextSource>,
    new: &dyn TextSource,
    styles: &mut StyleConfigMap,
) -> Result<(), KitError> {
    if let (Some(old), Some(k)) = (old, new.key())
        && old.key() == Some(k)
    {
        return Ok(());
    }
    let text = w.doc.get_text(key);
    let new_text = new.text();
    let new_runs = new.runs();
    check_runs(new_text, &new_runs)?;
    let new_paras = new.paragraphs();
    for attrs in new_paras.iter() {
        for (k, v) in attrs {
            crate::props::check_key(k)?;
            v.check()?;
        }
    }
    // What Loro holds: the old model, unless it is out of step (then Loro's own).
    let stored;
    let (old_text, old_runs, old_paras): (Cow<'_, str>, Cow<'_, [Run]>, Cow<'_, [Attrs]>) = match old {
        Some(o) if text.len_utf8() == o.text().len() => (Cow::Borrowed(o.text()), o.runs(), o.paragraphs()),
        _ => {
            stored = read(w.doc, key);
            (Cow::Owned(stored.text), Cow::Owned(stored.runs), Cow::Owned(stored.paragraphs))
        }
    };
    let p = common_prefix(&old_text, new_text);
    let s = common_suffix(&old_text, new_text, old_text.len().min(new_text.len()) - p);
    let (old_end, new_end) = (old_text.len() - s, new_text.len() - s);
    // The text.
    if old_end > p {
        w.spend(old_end - p)?;
        text.delete_utf8(p, old_end - p)?;
    }
    let mut at = p;
    while at < new_end {
        let mut room = w.room();
        if room == 0 {
            w.cut()?;
            room = w.room();
        }
        let mut end = (at + room.max(1)).min(new_end);
        while end > at && !new_text.is_char_boundary(end) {
            end -= 1;
        }
        if end == at {
            // One character bigger than the room: it goes alone.
            end = at + new_text.get(at..).and_then(|t| t.chars().next()).map_or(1, char::len_utf8);
        }
        let piece = new_text.get(at..end).ok_or_else(|| KitError::Invalid("the text's edit isn't on character boundaries".into()))?;
        w.spend(piece.len())?;
        text.insert_utf8(at, piece)?;
        at = end;
    }
    // Character attributes.
    let middle = stored_runs(&text, p, new_end)?;
    let current = moved_runs(&old_runs, p, old_end, new_end, middle);
    apply(w, &text, styles, differences(&new_runs, &current), "")?;
    // Paragraph attributes, on the `\n`s.
    // (Paragraphs the app didn't give attributes for have none.)
    let new_count = new_text.matches('\n').count();
    let empty = Attrs::new();
    let want_at = |j: usize| new_paras.get(j).unwrap_or(&empty);
    let inserted = new_text.get(p..new_end).map_or(0, |t| t.matches('\n').count());
    let removed = old_text.get(p..old_end).map_or(0, |t| t.matches('\n').count());
    let mut differ: Vec<usize> = Vec::new();
    if inserted == 0 && removed == 0 {
        for j in 0..new_count {
            if old_paras.get(j).unwrap_or(&empty) != want_at(j) {
                differ.push(j);
            }
        }
    } else {
        let before = new_text.get(..p).map_or(0, |t| t.matches('\n').count());
        for j in 0..new_count {
            let want = want_at(j);
            let have = if j < before {
                old_paras.get(j)
            } else if j < before + inserted {
                None
            } else {
                old_paras.get(j + removed - inserted)
            };
            if have.unwrap_or(&empty) != want {
                differ.push(j);
            }
        }
    }
    if !differ.is_empty() {
        let positions = breaks(new_text);
        let mut desired = Vec::new();
        let mut current = Vec::new();
        for j in differ {
            let Some(&x) = positions.get(j) else { continue };
            let range = x..x + 1;
            desired.push((range.clone(), want_at(j).clone()));
            // What the `\n` holds now.
            let mut held = Attrs::new();
            for delta in text.slice_delta(x, x + 1, PosType::Bytes)? {
                if let TextDelta::Insert { attributes, .. } = delta {
                    held = split_attrs(attributes.iter().flatten()).1;
                }
            }
            current.push((range, held));
        }
        apply(w, &text, styles, differences(&desired, &current), &PARA.to_string())?;
    }
    // The last paragraph's.
    let last = w.doc.get_map(last_name(key));
    let want = new_paras.get(new_count).cloned().unwrap_or_default();
    for (k, v) in &want {
        w.set(&last, k, v)?;
    }
    let mut stale = Vec::new();
    last.for_each(|k, _| {
        if !want.contains_key(k) {
            stale.push(k.to_owned());
        }
    });
    for k in stale {
        w.delete(&last, &k)?;
    }
    Ok(())
}

/// The text `key` as stored.
pub(crate) fn read(doc: &LoroDoc, key: &str) -> RichText {
    let text = doc.get_text(key);
    let mut out = RichText::default();
    for delta in text.to_delta() {
        let TextDelta::Insert { insert, attributes } = delta else { continue };
        let start = out.text.len();
        let (chars, paras) = split_attrs(attributes.iter().flatten());
        for _ in insert.matches('\n') {
            out.paragraphs.push(paras.clone());
        }
        out.text.push_str(&insert);
        let end = out.text.len();
        if chars.is_empty() {
            continue;
        }
        match out.runs.last_mut() {
            Some((r, a)) if r.end == start && *a == chars => r.end = end,
            _ => out.runs.push((start..end, chars)),
        }
    }
    out.paragraphs.push(last_paragraph(doc, key));
    out
}

/// The last paragraph's attributes, as stored beside the text.
fn last_paragraph(doc: &LoroDoc, key: &str) -> Attrs {
    let mut last = Attrs::new();
    doc.get_map(last_name(key)).for_each(|k, v| {
        if let ValueOrContainer::Value(v) = v
            && let Some(v) = Value::from_loro(&v)
        {
            last.insert(k.to_owned(), v);
        }
    });
    last
}

/// The byte `chars` characters after byte `from` of `text`; `None` past its end.
fn byte_after(text: &str, from: usize, chars: usize) -> Option<usize> {
    if chars == 0 {
        return Some(from);
    }
    let bytes = text.as_bytes().get(from..)?;
    let mut seen = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        // A character starts at every byte that isn't a continuation byte.
        if b & 0xC0 != 0x80 {
            if seen == chars {
                return Some(from + i);
            }
            seen += 1;
        }
    }
    (seen == chars).then_some(text.len())
}

/// Sets or removes attribute changes (Loro's: a null value removes) on `range` of `runs` (sorted), splitting runs.
fn change_runs(runs: Vec<(Range<usize>, Attrs)>, range: Range<usize>, changes: &Attrs, removed: &[String]) -> Vec<(Range<usize>, Attrs)> {
    let mut out = Vec::with_capacity(runs.len() + 2);
    let mut at = range.start;
    let apply = |attrs: &mut Attrs| {
        for (k, v) in changes {
            attrs.insert(k.clone(), v.clone());
        }
        for k in removed {
            attrs.remove(k);
        }
    };
    for (r, attrs) in runs {
        if r.end <= range.start || r.start >= range.end {
            out.push((r, attrs));
            continue;
        }
        // A gap before this run inside the range.
        if r.start > at {
            let mut a = Attrs::new();
            apply(&mut a);
            out.push((at..r.start, a));
        }
        if r.start < range.start {
            out.push((r.start..range.start, attrs.clone()));
        }
        let mut inside = attrs.clone();
        apply(&mut inside);
        out.push((r.start.max(range.start)..r.end.min(range.end), inside));
        if r.end > range.end {
            out.push((range.end..r.end, attrs));
        }
        at = r.end.min(range.end);
    }
    if at < range.end {
        let mut a = Attrs::new();
        apply(&mut a);
        out.push((at..range.end, a));
    }
    out.sort_by_key(|(r, _)| r.start);
    tidy(out)
}

/// Adjacent runs with the same attributes merged, empty ones dropped.
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

/// Loro's attribute changes on a retain, char and paragraph ones apart: what to set, and what a null removes.
fn changes<'a>(attributes: impl IntoIterator<Item = (&'a String, &'a loro::LoroValue)>) -> ((Attrs, Vec<String>), (Attrs, Vec<String>)) {
    let (mut chars, mut paras) = ((Attrs::new(), Vec::new()), (Attrs::new(), Vec::new()));
    for (k, v) in attributes {
        let (target, name) = match k.strip_prefix(PARA) {
            Some(name) => (&mut paras, name.to_owned()),
            None => (&mut chars, k.clone()),
        };
        match Value::from_loro(v) {
            Some(Value::Null) | None => target.1.push(name),
            Some(v) => {
                target.0.insert(name, v);
            }
        }
    }
    (chars, paras)
}

/// `old` (the text as the model had it before the change) with Loro's event `deltas` applied, in order: only what
/// changed is touched (a remote keystroke in a 1 MB story copies the text once and moves its runs, without reading the
/// whole text back from Loro). `None` when the deltas don't fit `old` (the model was out of step): read it whole.
pub(crate) fn patch(doc: &LoroDoc, key: &str, old: &dyn TextSource, deltas: &[Vec<TextDelta>]) -> Option<RichText> {
    let mut text = old.text().to_owned();
    let mut runs: Vec<(Range<usize>, Attrs)> = old.runs().into_owned();
    let mut paras: Vec<Attrs> = old.paragraphs().into_owned();
    paras.resize(text.matches('\n').count() + 1, Attrs::new());
    paras.pop();
    for delta in deltas {
        let mut out = String::with_capacity(text.len() + 64);
        // (old byte range, new byte start) of every piece kept, to move the runs.
        let mut kept: Vec<(Range<usize>, usize)> = Vec::new();
        let mut added: Vec<(Range<usize>, Attrs)> = Vec::new();
        let mut marked: Vec<(Range<usize>, Attrs, Vec<String>)> = Vec::new();
        let mut new_paras: Vec<Attrs> = Vec::with_capacity(paras.len() + 1);
        let (mut ob, mut pi) = (0usize, 0usize);
        for op in delta {
            match op {
                TextDelta::Retain { retain, attributes } => {
                    let end = byte_after(&text, ob, *retain)?;
                    let piece = text.get(ob..end)?;
                    let start = out.len();
                    out.push_str(piece);
                    kept.push((ob..end, start));
                    let breaks = piece.matches('\n').count();
                    let ((set, unset), (pset, punset)) = changes(attributes.iter().flatten());
                    for _ in 0..breaks {
                        let mut p = paras.get(pi)?.clone();
                        for (k, v) in &pset {
                            p.insert(k.clone(), v.clone());
                        }
                        for k in &punset {
                            p.remove(k);
                        }
                        new_paras.push(p);
                        pi += 1;
                    }
                    if !set.is_empty() || !unset.is_empty() {
                        marked.push((start..out.len(), set, unset));
                    }
                    ob = end;
                }
                TextDelta::Insert { insert, attributes } => {
                    let start = out.len();
                    out.push_str(insert);
                    let (chars, ps) = split_attrs(attributes.iter().flatten());
                    for _ in insert.matches('\n') {
                        new_paras.push(ps.clone());
                    }
                    if !chars.is_empty() {
                        added.push((start..out.len(), chars));
                    }
                }
                TextDelta::Delete { delete } => {
                    let end = byte_after(&text, ob, *delete)?;
                    pi += text.get(ob..end)?.matches('\n').count();
                    ob = end;
                }
            }
        }
        // What the delta leaves out is kept as it is.
        let rest = text.get(ob..)?;
        let start = out.len();
        out.push_str(rest);
        kept.push((ob..text.len(), start));
        new_paras.extend(paras.get(pi..)?.iter().cloned());
        // The runs, moved with the pieces they were in.
        let mut moved: Vec<(Range<usize>, Attrs)> = Vec::with_capacity(runs.len() + added.len());
        let mut k = 0;
        for (r, a) in runs {
            while kept.get(k).is_some_and(|(o, _)| o.end <= r.start) {
                k += 1;
            }
            let mut j = k;
            while let Some((o, n)) = kept.get(j) {
                if o.start >= r.end {
                    break;
                }
                let (s, e) = (r.start.max(o.start), r.end.min(o.end));
                if s < e {
                    moved.push((s - o.start + n..e - o.start + n, a.clone()));
                }
                j += 1;
            }
        }
        moved.extend(added);
        moved.sort_by_key(|(r, _)| r.start);
        runs = tidy(moved);
        for (range, set, unset) in marked {
            runs = change_runs(runs, range, &set, &unset);
        }
        text = out;
        paras = new_paras;
    }
    // Out of step with what Loro holds: no answer.
    if text.len() != doc.get_text(key).len_utf8() || paras.len() != text.matches('\n').count() {
        return None;
    }
    paras.push(last_paragraph(doc, key));
    Some(RichText { text, runs, paragraphs: paras })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use loro::{ExportMode, LoroDoc};

    use super::*;

    /// A text patched from Loro's deltas is the text read back whole: inserts, deletes and marks among characters of
    /// one to four bytes, paragraph marks, a deleted break, several updates in one go.
    #[test]
    fn a_patched_text_is_the_text_read_whole() {
        let a = LoroDoc::new();
        a.config_default_text_style(Some(StyleConfig { expand: ExpandType::After }));
        let mut styles = StyleConfigMap::new();
        configure(&a, &mut styles, "¶h");
        let t = a.get_text("t");
        t.insert(0, "héllo 日本\nwörld 🙂 end\nlast").unwrap();
        a.commit();
        let b = LoroDoc::new();
        b.import(&a.export(ExportMode::Snapshot).unwrap()).unwrap();
        let deltas: Arc<Mutex<Vec<Vec<TextDelta>>>> = Arc::default();
        let d = deltas.clone();
        let _sub = b.subscribe_root(Arc::new(move |e| {
            for diff in e.events {
                if let loro::event::Diff::Text(delta) = diff.diff {
                    d.lock().unwrap().push(delta.clone());
                }
            }
        }));
        let edits: Vec<Box<dyn Fn(&LoroText)>> = vec![
            Box::new(|t| t.insert(3, "XY🙂").unwrap()),
            Box::new(|t| t.mark(2..9, "b", true).unwrap()),
            Box::new(|t| t.mark(10..11, "¶h", 1i64).unwrap()),
            Box::new(|t| t.delete(9, 3).unwrap()),
            Box::new(|t| t.unmark(4..6, "b").unwrap()),
            Box::new(|t| t.insert(0, "日\n").unwrap()),
        ];
        let mut sent = Vec::new();
        for edit in &edits {
            let before = a.oplog_vv();
            edit(&a.get_text("t"));
            a.commit();
            sent.push(a.export(ExportMode::updates(&before)).unwrap());
        }
        // Two updates at a time, then the rest.
        for chunk in sent.chunks(2) {
            let old = read(&b, "t");
            deltas.lock().unwrap().clear();
            for u in chunk {
                b.import(u).unwrap();
            }
            let got = patch(&b, "t", &old, &deltas.lock().unwrap()).expect("patched");
            assert_eq!(got, read(&b, "t"));
        }
        // Deltas that don't fit the old text: no answer (read it whole).
        let wrong = RichText::plain("short");
        assert!(patch(&b, "t", &wrong, &[vec![TextDelta::Retain { retain: 999, attributes: None }]]).is_none());
    }
}
