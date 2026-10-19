//! The app on eframe: [`App::logic`] (which eframe calls also while the window is hidden in the tray) feeds the
//! link's answers, the clock and outside events (a second start, the tray) to the model and carries out its effects;
//! [`App::ui`] draws the window ([`crate::ui::show`]) and feeds its clicks to the model. Nothing here waits: agent calls
//! run on the link's threads, the folder picker on a thread of its own.

use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use egui::ViewportCommand;

use crate::agent::{Agent, Link, Starter};
use crate::model::{Effect, Event, Model, Screen};

/// The running app.
pub struct App {
    model: Model,
    link: Link,
    /// Events from outside the window: a second start ("show"), the tray.
    outside: Receiver<Event>,
    /// Where the folder picker's answer goes.
    to_self: Sender<Event>,
    /// Whether the window was last told to be shown.
    shown: bool,
    /// The app is quitting: the window's close goes through.
    closing: bool,
    /// Carries out the effects that aren't agent calls (the OS's own: the opener, the folder picker, the app's
    /// start-at-login entry, the tray).
    host: Box<dyn Host>,
}

/// What the app asks of the OS, apart from the agent.
pub trait Host {
    /// Opens `folder` in the file manager.
    fn open_folder(&self, folder: &str);
    /// Writes (`true`) or removes the app's own start-at-login entry.
    fn set_autostart(&self, on: bool) -> Result<(), String>;
    /// Shows the OS's folder picker at `start`, off the UI thread; the answer is sent to `answer` (then `wake`).
    fn pick_folder(&self, start: String, answer: Sender<Event>, wake: Arc<dyn Fn() + Send + Sync>);
    /// Ends the tray process (Linux, where the tray is a process of its own).
    fn quit_tray(&self) {}
    /// Called every frame with the model, to show its state (the tray icon); events it has go to the model.
    fn frame(&mut self, _model: &Model) -> Vec<Event> {
        Vec::new()
    }
}

impl App {
    /// The app for `model`, its agent calls going to `agent` (started with `start` when it doesn't run). `outside`
    /// brings events from a second start and the tray; `outside_tx` is its sender, for the folder picker's answer.
    pub fn new(
        ctx: &egui::Context,
        agent: Arc<dyn Agent>,
        start: Starter,
        model: Model,
        outside: (Sender<Event>, Receiver<Event>),
        host: Box<dyn Host>,
    ) -> Self {
        let wake_ctx = ctx.clone();
        let link = Link::new(agent, start, Arc::new(move || wake_ctx.request_repaint()));
        link.start_clock(crate::now_ms);
        let shown = model.window_visible;
        let mut app = Self { model, link, outside: outside.1, to_self: outside.0, shown, closing: false, host };
        let first = app.model.update(Event::Start);
        app.carry_out(first, ctx);
        app
    }

    fn handle(&mut self, event: Event, ctx: &egui::Context) {
        let effects = self.model.update(event);
        self.carry_out(effects, ctx);
    }

    fn carry_out(&mut self, effects: Vec<Effect>, ctx: &egui::Context) {
        for effect in effects {
            match self.link.perform(effect) {
                None => {}
                Some(Effect::OpenFolder(folder)) => self.host.open_folder(&folder),
                Some(Effect::QuitTray) => self.host.quit_tray(),
                Some(Effect::QuitApp) => {
                    self.closing = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
                Some(Effect::AppAutostart(on)) => {
                    let done = self.host.set_autostart(on);
                    self.handle(Event::AppAutostartSet(done), ctx);
                }
                Some(Effect::PickFolder(start)) => {
                    let wake_ctx = ctx.clone();
                    self.host.pick_folder(start, self.to_self.clone(), Arc::new(move || wake_ctx.request_repaint()));
                }
                // Agent calls never come back from the link.
                Some(_) => {}
            }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        for event in self.link.events() {
            self.handle(event, ctx);
        }
        let outside: Vec<Event> = self.outside.try_iter().collect();
        for event in outside {
            self.handle(event, ctx);
        }
        for event in self.host.frame(&self.model) {
            self.handle(event, ctx);
        }
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            self.handle(Event::CloseRequested, ctx);
            if !self.closing {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            }
        }
        if self.model.window_visible != self.shown {
            self.shown = self.model.window_visible;
            ctx.send_viewport_cmd(ViewportCommand::Visible(self.shown));
            if self.shown {
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
        }
        // The add-a-computer countdown moves every second.
        if matches!(self.model.screen, Screen::AddingComputer(_)) {
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let events = crate::ui::show(ui, &mut self.model, crate::now_ms());
        let ctx = ui.ctx().clone();
        for event in events {
            self.handle(event, &ctx);
        }
    }
}

/// The OS's side, for real: the tray icon (where there is one), the opener, the folder picker, the app's own start
/// at login.
pub struct System {
    pub tray: Option<Box<dyn crate::tray::Tray>>,
    /// Linux: the single-instance folder, where the tray process is told to end at Quit.
    pub tray_process: Option<std::path::PathBuf>,
}

impl Host for System {
    fn quit_tray(&self) {
        if let Some(dir) = &self.tray_process {
            let _ = crate::single_instance::send(dir, crate::single_instance::Name::Tray, crate::single_instance::Command::Quit);
        }
    }

    fn frame(&mut self, model: &Model) -> Vec<Event> {
        if let Some(tray) = &mut self.tray {
            let paused = model.status.as_ref().is_some_and(|s| s.run_state == craft_cloud_app::RunState::PausedByUser);
            tray.show(crate::tray::state_of(model.status.as_ref()), paused);
        }
        Vec::new()
    }

    fn open_folder(&self, folder: &str) {
        open_folder(folder);
    }

    fn set_autostart(&self, on: bool) -> Result<(), String> {
        crate::autostart::set_for_this_app(on)
    }

    fn pick_folder(&self, start: String, answer: Sender<Event>, wake: Arc<dyn Fn() + Send + Sync>) {
        let _ = std::thread::Builder::new().name("craft-cloud-picker".into()).spawn(move || {
            let mut dialog = rfd::AsyncFileDialog::new().set_title("The folder to sync");
            if std::path::Path::new(&start).is_dir() {
                dialog = dialog.set_directory(&start);
            }
            let picked = block_on(dialog.pick_folder()).map(|handle| handle.path().display().to_string());
            let _ = answer.send(Event::FolderPicked(picked));
            wake();
        });
    }
}

/// Opens `folder` in the file manager (`explorer`, `open`, `xdg-open`), in a process of its own that nothing waits
/// for.
pub fn open_folder(folder: &str) {
    let opener = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_vendor = "apple") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = crate::start_agent::spawn_detached(std::path::Path::new(opener), &[folder]);
}

/// Runs `future` to its end on this thread (the folder picker's, which has nothing else to do meanwhile).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};

    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}
