//! A small model for the kit's tests: a title (an opaque value) and a list of notes (a table with one column), plus
//! helpers to drive sessions on the fake agent.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_cloud_app::fake::Fake;
use craft_cloud_kit::{Agent, ElemId, KitError, LiveDocument, Mappings, Opaque, Open, RemotePatch, Session, TableSource, Value};

/// The one column of the list.
pub const TEXT: ElemId = ElemId(1);

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Notes {
    pub title: Arc<String>,
    pub items: Arc<Vec<(ElemId, String)>>,
    /// Cells known changed since the previous model (the hint), when the change says.
    pub hint: Option<Vec<ElemId>>,
}

impl Notes {
    pub fn new(title: &str, items: &[&str]) -> Self {
        Self { title: Arc::new(title.into()), items: Arc::new(items.iter().map(|t| (ElemId::new(), (*t).to_owned())).collect()), hint: None }
    }

    pub fn texts(&self) -> Vec<String> {
        self.items.iter().map(|(_, t)| t.clone()).collect()
    }

    pub fn with_title(&self, title: &str) -> Self {
        Self { title: Arc::new(title.into()), hint: None, ..self.clone() }
    }

    pub fn with_item(&self, at: usize, text: &str) -> Self {
        let mut items = (*self.items).clone();
        let id = ElemId::new();
        items.insert(at.min(items.len()), (id, text.to_owned()));
        Self { items: Arc::new(items), hint: Some(vec![id]), ..self.clone() }
    }

    pub fn with_edit(&self, at: usize, text: &str) -> Self {
        let mut items = (*self.items).clone();
        let mut hint = Vec::new();
        if let Some(item) = items.get_mut(at) {
            item.1 = text.to_owned();
            hint.push(item.0);
        }
        Self { items: Arc::new(items), hint: Some(hint), ..self.clone() }
    }

    pub fn without(&self, at: usize) -> Self {
        let mut items = (*self.items).clone();
        if at < items.len() {
            items.remove(at);
        }
        Self { items: Arc::new(items), hint: Some(Vec::new()), ..self.clone() }
    }
}

pub struct Items<'a>(&'a Notes);

impl TableSource for Items<'_> {
    fn key(&self) -> Option<usize> {
        Some(Arc::as_ptr(&self.0.items) as usize)
    }

    fn rows(&self) -> Vec<ElemId> {
        self.0.items.iter().map(|(id, _)| *id).collect()
    }

    fn columns(&self) -> Vec<ElemId> {
        vec![TEXT]
    }

    fn cell(&self, row: ElemId, column: ElemId) -> Option<Value> {
        if column != TEXT {
            return None;
        }
        self.0.items.iter().find(|(id, _)| *id == row).map(|(_, t)| Value::from(t.as_str()))
    }

    fn cells(&self) -> Vec<(ElemId, ElemId, Value)> {
        self.0.items.iter().map(|(id, t)| (*id, TEXT, Value::from(t.as_str()))).collect()
    }

    fn changed_cells(&self) -> Option<Vec<(ElemId, ElemId)>> {
        self.0.hint.as_ref().map(|ids| ids.iter().map(|id| (*id, TEXT)).collect())
    }
}

/// The notes app, at layout version `V`.
pub struct NotesApp<const V: u32>;

impl<const V: u32> LiveDocument for NotesApp<V> {
    type Doc = Notes;
    const APP: &'static str = "notes";
    const SCHEMA: u32 = V;

    fn mappings(doc: &Notes) -> Mappings<'_> {
        Mappings::new().opaque("title", Opaque { value: Some(&*doc.title), key: Some(Arc::as_ptr(&doc.title) as usize) }).table("items", Items(doc))
    }

    fn apply_remote(old: &Arc<Notes>, patch: &RemotePatch<'_>) -> Result<Arc<Notes>, KitError> {
        let mut new = (**old).clone();
        new.hint = None;
        if let Some(title) = patch.opaque("title") {
            new.title = Arc::new(String::from_utf8_lossy(&title.unwrap_or_default()).into_owned());
        }
        if let Some(table) = patch.table("items") {
            let items = table
                .rows()
                .into_iter()
                .map(|row| (row, table.cell(row, TEXT).and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()))
                .collect();
            new.items = Arc::new(items);
        }
        Ok(Arc::new(new))
    }

    fn normalise(_doc: &mut Notes) {}

    fn refresh_derived(_doc: &mut Notes) {}
}

pub type App = NotesApp<1>;

pub fn agent(fake: &Fake) -> Agent {
    let client = fake.client();
    Agent::from_fn(move || Ok(client.clone()))
}

/// Opens `path` live on `fake`'s machine (importing `file` if it was never live).
pub fn open<D: LiveDocument>(fake: &Fake, path: &str, file: D::Doc) -> Session<D> {
    match Session::<D>::open(&agent(fake), path, Arc::new(file), None).unwrap() {
        Open::Live(s) => *s,
        Open::NotLive(why) => panic!("not live: {why:?}"),
    }
}

/// Polls every session until `done` holds (10 s at most).
pub fn until<D: LiveDocument>(sessions: &mut [&mut Session<D>], what: &str, mut done: impl FnMut(&[&mut Session<D>]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        for s in sessions.iter_mut() {
            s.poll();
        }
        if done(sessions) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Makes a change from the session's model.
pub fn change<D: LiveDocument>(s: &mut Session<D>, f: impl FnOnce(&D::Doc) -> D::Doc) -> Result<(), KitError> {
    let old = s.model().clone();
    let new = Arc::new(f(&old));
    s.local_change(&old, &new)
}
