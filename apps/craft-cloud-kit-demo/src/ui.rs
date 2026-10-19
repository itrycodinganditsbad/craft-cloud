//! The demo's window: the account line and its recents, the document's badge, the story (a text box with bold and
//! headings, other machines' cursors marked) and the layer tree beside it (add, rename, move, delete).
//!
//! The kit's hook is [`Demo::apply`]: every command ends there with the model before and after it. Each frame
//! [`Demo::show`] polls the session first. While the document isn't live (opening, or Craft Cloud isn't there) the
//! demo keeps its own snapshot undo; once live, undo goes through the session.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use craft_cloud_kit::{Agent, ElemId, NotLive, Open, Session, UndoKind, Value};
use egui::text::LayoutJob;
use egui::{Color32, FontId, TextFormat, Ui};

use crate::model::{self, BOLD, DemoApp, DemoDoc, HEADING, Layer};

/// How long after a change the demo saves by itself.
pub const AUTOSAVE_AFTER: Duration = Duration::from_secs(5);
/// How often, and how many times, opening is tried while the file isn't synced yet.
const OPEN_RETRY: Duration = Duration::from_secs(2);
const OPEN_TRIES: u32 = 60;

type Opened = Result<Open<DemoApp>, String>;

/// Where the document stands.
enum Mode {
    /// Being opened live, off the UI thread.
    Opening(Receiver<Opened>),
    Live(Box<Session<DemoApp>>),
    /// A plain file (the reason): the demo's own undo.
    Local(String),
}

/// What the window shows of other machines' cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub machine: String,
    pub at: usize,
}

/// The demo.
pub struct Demo {
    /// The file, absolute, and as the agent knows it (relative to the synced folder).
    file: PathBuf,
    rel: String,
    machine: String,
    mode: Mode,
    model: Arc<DemoDoc>,
    /// The demo's own undo, while not live.
    undo: Vec<Arc<DemoDoc>>,
    redo: Vec<Arc<DemoDoc>>,
    selected: Option<ElemId>,
    rename: String,
    cursor: Option<(usize, usize)>,
    dirty_since: Option<Instant>,
    pub autosave: bool,
    /// The last message for the status line.
    pub message: String,
    watch: Option<craft_cloud_egui::CloudWatch>,
    /// The file's model and bytes as read at open (to bring a file changed outside Craft in).
    read: (Arc<DemoDoc>, Vec<u8>),
    composing: bool,
    /// The user chose what to do with a file changed outside Craft.
    outside_handled: bool,
}

/// Reads the file at `path` (an empty document if there is none yet); its bytes too.
pub fn read_file(path: &Path) -> Result<(DemoDoc, Vec<u8>), String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok((model::from_file(&bytes)?, bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((DemoDoc::default(), Vec::new())),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Writes `bytes` to `path` atomically (a temporary file beside it, then renamed).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

impl Demo {
    /// Opens `rel` (relative to the synced folder `folder`) as `machine`, live through `agent` (opened off this
    /// thread; `wake` is called when it is open or something comes).
    pub fn open(agent: Option<Agent>, folder: &Path, rel: &str, machine: &str, wake: impl Fn() + Send + Sync + Clone + 'static) -> Self {
        let file = folder.join(rel);
        let (doc, bytes, message) = match read_file(&file) {
            Ok((doc, bytes)) => (doc, bytes, String::new()),
            Err(e) => (DemoDoc::default(), Vec::new(), e),
        };
        let mut doc = doc;
        <DemoApp as craft_cloud_kit::LiveDocument>::refresh_derived(&mut doc);
        let model = Arc::new(doc);
        // A new file is written once, so the file sync has something to make live.
        if bytes.is_empty() && !file.exists() {
            let _ = write_atomic(&file, &model::to_file(&model));
        }
        let bytes = std::fs::read(&file).unwrap_or(bytes);
        let mode = match agent.clone() {
            None => Mode::Local("Craft Cloud isn't running on this computer".into()),
            Some(agent) => {
                let (tx, rx) = std::sync::mpsc::channel();
                let (rel, file_model, file_bytes, w) = (rel.to_owned(), model.clone(), bytes.clone(), wake.clone());
                let _ = std::thread::Builder::new().name("kit-demo-open".into()).spawn(move || {
                    let mut tries = 0;
                    let result = loop {
                        tries += 1;
                        match Session::<DemoApp>::open(&agent, &rel, file_model.clone(), Some(&file_bytes)) {
                            Ok(Open::NotLive(NotLive::Reason(craft_cloud_app::live::NotLiveReason::NotSynced))) if tries < OPEN_TRIES => {
                                std::thread::sleep(OPEN_RETRY);
                            }
                            other => break other.map_err(|e| e.to_string()),
                        }
                    };
                    let _ = tx.send(result);
                    w();
                });
                Mode::Opening(rx)
            }
        };
        let watch = agent.map(|a| {
            let w = wake.clone();
            craft_cloud_egui::CloudWatch::start("kitdemo", move || a.client(), w)
        });
        Self {
            file,
            rel: rel.to_owned(),
            machine: machine.to_owned(),
            mode,
            read: (model.clone(), bytes),
            model,
            undo: Vec::new(),
            redo: Vec::new(),
            selected: None,
            rename: String::new(),
            cursor: None,
            dirty_since: None,
            autosave: true,
            message,
            watch,
            composing: false,
            outside_handled: false,
        }
    }

    pub fn model(&self) -> &Arc<DemoDoc> {
        &self.model
    }

    pub fn is_live(&self) -> bool {
        matches!(self.mode, Mode::Live(_))
    }

    pub fn session(&self) -> Option<&Session<DemoApp>> {
        match &self.mode {
            Mode::Live(s) => Some(s),
            _ => None,
        }
    }

    /// Takes what came (the open, remote changes) without drawing: what each frame does first.
    pub fn poll(&mut self) {
        if let Mode::Opening(rx) = &self.mode {
            match rx.try_recv() {
                Ok(Ok(Open::Live(session))) => {
                    self.model = session.model().clone();
                    self.undo.clear();
                    self.redo.clear();
                    if let Some(w) = &self.watch {
                        w.add_recent(&self.rel, false);
                    }
                    self.mode = Mode::Live(session);
                }
                Ok(Ok(Open::NotLive(why))) => self.mode = Mode::Local(format!("not live: {why:?}")),
                Ok(Err(e)) => self.mode = Mode::Local(e),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.mode = Mode::Local("the open stopped".into()),
            }
        }
        if let Mode::Live(session) = &mut self.mode {
            session.hold_remote(self.composing);
            if session.poll().changed {
                self.model = session.model().clone();
                if self.selected.is_some_and(|id| model::find(&self.model.layers, id).is_none()) {
                    self.selected = None;
                }
            }
        }
    }

    /// The hook: a command made `new` from the current model. Live, the session writes the difference (one undo
    /// step) and sends it; otherwise the demo keeps the old model for its own undo.
    pub fn apply(&mut self, mut new: DemoDoc) {
        <DemoApp as craft_cloud_kit::LiveDocument>::refresh_derived(&mut new);
        let new = Arc::new(new);
        let old = self.model.clone();
        if *new == *old {
            return;
        }
        match &mut self.mode {
            Mode::Live(session) => match session.local_change(&old, &new) {
                Ok(()) => self.model = new,
                Err(e) => {
                    self.message = e.to_string();
                    // (Read-only, stalled or out of step: the session's model stands.)
                    self.model = session.model().clone();
                }
            },
            _ => {
                self.undo.push(old);
                self.redo.clear();
                self.model = new;
            }
        }
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    pub fn undo(&mut self) {
        match &mut self.mode {
            Mode::Live(session) => {
                if let Err(e) = session.undo() {
                    self.message = e.to_string();
                }
                self.model = session.model().clone();
            }
            _ => {
                if let Some(prev) = self.undo.pop() {
                    self.redo.push(std::mem::replace(&mut self.model, prev));
                }
            }
        }
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    pub fn redo(&mut self) {
        match &mut self.mode {
            Mode::Live(session) => {
                if let Err(e) = session.redo() {
                    self.message = e.to_string();
                }
                self.model = session.model().clone();
            }
            _ => {
                if let Some(next) = self.redo.pop() {
                    self.undo.push(std::mem::replace(&mut self.model, next));
                }
            }
        }
        self.dirty_since.get_or_insert_with(Instant::now);
    }

    /// Writes the file (atomically) and, live, tells the session (the save record).
    pub fn save(&mut self) {
        let bytes = model::to_file(&self.model);
        if let Err(e) = write_atomic(&self.file, &bytes) {
            self.message = format!("not saved: {e}");
            return;
        }
        if let Mode::Live(session) = &mut self.mode
            && let Err(e) = session.saved(&bytes)
        {
            self.message = e.to_string();
        }
        if let Some(w) = &self.watch {
            w.add_recent(&self.rel, true);
        }
        self.dirty_since = None;
        self.message = "saved".into();
    }

    /// The other machines' cursors in the story.
    pub fn cursors(&self) -> Vec<Cursor> {
        let Some(session) = self.session() else { return Vec::new() };
        let mut out: Vec<Cursor> = session
            .peers()
            .values()
            .filter_map(|p| {
                let text = std::str::from_utf8(&p.bytes).ok()?;
                let (machine, at) = text.split_once('@')?;
                Some(Cursor { machine: machine.to_owned(), at: at.parse().ok()? })
            })
            .collect();
        out.sort_by(|a, b| a.machine.cmp(&b.machine));
        out
    }

    /// The layer to act on, or the first.
    fn target(&self) -> Option<ElemId> {
        self.selected.or_else(|| self.model.layers.first().map(|l| l.id))
    }

    pub fn add_layer(&mut self, name: &str) -> ElemId {
        let id = ElemId::new();
        let layer = Arc::new(Layer { id, name: name.to_owned(), children: Vec::new() });
        let layers = match self.selected {
            Some(parent) => model::insert(&self.model.layers, Some(parent), usize::MAX, layer),
            None => model::insert(&self.model.layers, None, usize::MAX, layer),
        };
        self.apply(DemoDoc { layers: Arc::new(layers), ..(*self.model).clone() });
        id
    }

    pub fn select(&mut self, id: Option<ElemId>) {
        self.selected = id;
        self.rename = id.and_then(|id| model::find(&self.model.layers, id)).map(|l| l.name.clone()).unwrap_or_default();
    }

    fn layers_op(&mut self, op: impl FnOnce(&[Arc<Layer>], ElemId) -> Vec<Arc<Layer>>) {
        let Some(id) = self.target() else { return };
        let layers = op(&self.model.layers, id);
        self.apply(DemoDoc { layers: Arc::new(layers), ..(*self.model).clone() });
    }

    pub fn rename_selected(&mut self, name: &str) {
        let name = name.to_owned();
        self.layers_op(|l, id| model::rename(l, id, &name));
    }

    /// The window's frame: poll, draw, autosave.
    pub fn show(&mut self, ui: &mut Ui) {
        self.poll();
        // Keys: undo and redo.
        let (undo, redo) = ui.input(|i| {
            let cmd = i.modifiers.command;
            (
                cmd && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                cmd && (i.key_pressed(egui::Key::Y) || (i.modifiers.shift && i.key_pressed(egui::Key::Z))),
            )
        });
        // An IME composition holds remote changes back until it ends.
        self.composing = ui.input(|i| {
            i.events.iter().fold(self.composing, |on, e| match e {
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => !text.is_empty(),
                egui::Event::Ime(egui::ImeEvent::Commit(_)) => false,
                _ => on,
            })
        });
        self.top(ui);
        ui.separator();
        ui.columns(2, |cols| {
            if let [story, layers] = cols {
                self.story(story);
                self.layers(layers);
            }
        });
        if undo {
            self.undo();
        }
        if redo {
            self.redo();
        }
        if self.autosave && self.dirty_since.is_some_and(|at| at.elapsed() >= AUTOSAVE_AFTER) {
            self.save();
        }
        if self.dirty_since.is_some() || self.is_live() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    fn top(&mut self, ui: &mut Ui) {
        if let Some(w) = &mut self.watch {
            craft_cloud_egui::account_line(ui, &w.state());
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&self.rel).strong());
            match &self.mode {
                Mode::Opening(_) => {
                    ui.weak("opening live…");
                }
                Mode::Live(session) => craft_cloud_egui::badge(ui, session.status()),
                Mode::Local(why) => {
                    ui.weak(format!("plain file: {why}"));
                }
            }
            ui.weak(format!("{} paragraphs", self.model.paragraphs));
            let live_undo = self.session().map(|s| (s.can_undo(), s.can_redo(), s.undo_kind()));
            let (can_undo, can_redo) = match live_undo {
                Some((u, r, UndoKind::Live)) => (u, r),
                Some((_, _, UndoKind::None)) => (false, false),
                None => (!self.undo.is_empty(), !self.redo.is_empty()),
            };
            if ui.add_enabled(can_undo, egui::Button::new("Undo")).clicked() {
                self.undo();
            }
            if ui.add_enabled(can_redo, egui::Button::new("Redo")).clicked() {
                self.redo();
            }
            if ui.button("Save").clicked() {
                self.save();
            }
            ui.checkbox(&mut self.autosave, "Autosave");
            if !self.message.is_empty() {
                ui.weak(&self.message);
            }
        });
        let outside = !self.outside_handled && self.session().is_some_and(|s| s.status().changed_outside);
        if outside {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::from_rgb(0xb0, 0x80, 0x10), "The file was changed outside Craft.");
                if ui.button("Bring it in").clicked() {
                    // The file's content as an ordinary local change (the difference of the live model and the
                    // file's), then saved, so the file and the document agree again.
                    let file = (*self.read.0).clone();
                    self.apply(file);
                    self.save();
                    self.outside_handled = true;
                }
                if ui.button("Keep the live document").clicked() {
                    // The next save writes the live content over the file.
                    self.outside_handled = true;
                    self.dirty_since.get_or_insert_with(Instant::now);
                }
            });
        }
        if let Some(ro) = self.session().and_then(|s| s.status().read_only.clone()) {
            ui.colored_label(Color32::from_rgb(0x40, 0x60, 0xb0), format!("Made with a newer kitdemo (layout {}): update it to edit.", ro.version));
        }
        if let Some(w) = &mut self.watch {
            let entries = w.entries().to_vec();
            ui.collapsing("Recent files", |ui| {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
                if let Some(path) = craft_cloud_egui::recents(ui, &entries, now) {
                    self.message = format!("open {path} with: craft-cloud-kit-demo \"{path}\"");
                }
            });
        }
    }

    fn story(&mut self, ui: &mut Ui) {
        let label = ui.label(egui::RichText::new("Story").strong());
        let editable = !matches!(self.mode, Mode::Opening(_)) && self.session().is_none_or(|s| s.status().read_only.is_none());
        ui.horizontal(|ui| {
            let range = self.cursor.map(|(a, b)| a.min(b)..a.max(b));
            if ui.add_enabled(editable && range.as_ref().is_some_and(|r| !r.is_empty()), egui::Button::new("Bold")).clicked()
                && let Some(r) = range
            {
                let on = model::has(&self.model.story, r.clone(), BOLD);
                let story = model::format(&self.model.story, r, BOLD, if on { None } else { Some(Value::Bool(true)) });
                self.apply(DemoDoc { story: Arc::new(story), ..(*self.model).clone() });
            }
            if ui.add_enabled(editable, egui::Button::new("Heading")).clicked() {
                let para = model::paragraph_at(&self.model.story, self.cursor.map_or(0, |c| c.0));
                let on = self.model.story.paragraphs.get(para).is_some_and(|p| p.contains_key(HEADING));
                let story = model::heading(&self.model.story, para, if on { None } else { Some(1) });
                self.apply(DemoDoc { story: Arc::new(story), ..(*self.model).clone() });
            }
            for c in self.cursors() {
                ui.colored_label(Color32::from_rgb(0xc0, 0x30, 0x90), format!("{} at {}", c.machine, c.at));
            }
        });
        let mut text = self.model.story.text.clone();
        let story = self.model.story.clone();
        let marks: Vec<usize> = self.cursors().iter().map(|c| c.at).collect();
        let mut layouter = move |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let mut job = layout(&story, buf.as_str(), &marks, ui);
            job.wrap.max_width = wrap;
            ui.fonts_mut(|f| f.layout_job(job))
        };
        let out =
            egui::TextEdit::multiline(&mut text).desired_width(f32::INFINITY).desired_rows(20).interactive(editable).layouter(&mut layouter).show(ui);
        let response = out.response.response.clone().labelled_by(label.id);
        if let Some(range) = out.cursor_range {
            let (a, b) = (range.primary.index.0, range.secondary.index.0);
            let to_byte = |chars: usize| text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i);
            let now = (to_byte(a), to_byte(b));
            if self.cursor != Some(now) {
                self.cursor = Some(now);
                if let Mode::Live(session) = &mut self.mode {
                    let _ = session.set_presence(Some(format!("{}@{}", self.machine, now.0).into_bytes()));
                }
            }
        }
        if response.changed() && text != self.model.story.text {
            let story = model::edit_text(&self.model.story, &text);
            self.apply(DemoDoc { story: Arc::new(story), ..(*self.model).clone() });
        }
    }

    fn layers(&mut self, ui: &mut Ui) {
        ui.label(egui::RichText::new("Layers").strong());
        let editable = !matches!(self.mode, Mode::Opening(_)) && self.session().is_none_or(|s| s.status().read_only.is_none());
        ui.add_enabled_ui(editable, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Add layer").clicked() {
                    let n = model::flatten(&self.model.layers).len() + 1;
                    let id = self.add_layer(&format!("Layer {n}"));
                    self.select(Some(id));
                }
                if ui.button("Up").clicked() {
                    self.layers_op(|l, id| model::shift(l, id, -1));
                }
                if ui.button("Down").clicked() {
                    self.layers_op(|l, id| model::shift(l, id, 1));
                }
                if ui.button("Indent").clicked() {
                    self.layers_op(model::indent);
                }
                if ui.button("Outdent").clicked() {
                    self.layers_op(model::outdent);
                }
                if ui.button("Delete").clicked() {
                    self.layers_op(|l, id| model::remove(l, id).0);
                    self.select(None);
                }
            });
            if self.selected.is_some() {
                ui.horizontal(|ui| {
                    let label = ui.label("Name");
                    let r = ui.text_edit_singleline(&mut self.rename).labelled_by(label.id);
                    if (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))) || ui.button("Rename").clicked() {
                        let name = self.rename.clone();
                        self.rename_selected(&name);
                    }
                });
            }
        });
        egui::ScrollArea::vertical().id_salt("layers").show(ui, |ui| {
            let mut clicked = None;
            for (depth, layer) in model::flatten(&self.model.layers) {
                ui.horizontal(|ui| {
                    ui.add_space(depth as f32 * 16.0);
                    if ui.selectable_label(self.selected == Some(layer.id), &layer.name).clicked() {
                        clicked = Some(layer.id);
                    }
                });
            }
            if let Some(id) = clicked {
                self.select(if self.selected == Some(id) { None } else { Some(id) });
            }
        });
    }
}

/// The story's text drawn with its runs (bold as strong and underlined), headings larger, and other machines'
/// cursors marked: one section per stretch that looks the same. `text` is the text box's (a frame ahead of the story
/// while typing: then it is drawn plain).
fn layout(story: &craft_cloud_kit::RichText, text: &str, marks: &[usize], ui: &Ui) -> LayoutJob {
    let mut job = LayoutJob::default();
    let normal = TextFormat { font_id: FontId::proportional(14.0), color: ui.visuals().text_color(), ..Default::default() };
    if story.text != text {
        job.append(text, 0.0, normal);
        return job;
    }
    // Where the look may change: runs, marks, and (when there are headings) paragraph breaks.
    let mut cuts: Vec<usize> = vec![0, text.len()];
    for (r, a) in &story.runs {
        if a.contains_key(BOLD) {
            cuts.extend([r.start, r.end]);
        }
    }
    for m in marks {
        let end = text.get(*m..).and_then(|t| t.chars().next()).map_or(*m, |c| m + c.len_utf8());
        cuts.extend([*m, end]);
    }
    let headings = story.paragraphs.iter().any(|p| p.contains_key(HEADING));
    if headings {
        cuts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
    }
    cuts.retain(|c| *c <= text.len() && text.is_char_boundary(*c));
    cuts.sort_unstable();
    cuts.dedup();
    for pair in cuts.windows(2) {
        let (Some(&a), Some(&b)) = (pair.first(), pair.get(1)) else { continue };
        let mut format = normal.clone();
        if story.runs.iter().any(|(r, attrs)| r.start <= a && a < r.end && attrs.contains_key(BOLD)) {
            format.color = ui.visuals().strong_text_color();
            format.underline = egui::Stroke::new(1.0, ui.visuals().strong_text_color());
        }
        if headings && story.paragraphs.get(model::paragraph_at(story, a)).is_some_and(|p| p.contains_key(HEADING)) {
            format.font_id = FontId::proportional(20.0);
        }
        if marks.contains(&a) {
            format.background = Color32::from_rgb(0xf0, 0xa0, 0xd0);
        }
        job.append(text.get(a..b).unwrap_or_default(), 0.0, format);
    }
    job
}
