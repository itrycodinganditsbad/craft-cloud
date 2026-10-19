//! Linux: the tray as a process of its own, `craft-cloud --tray`, without a window (Wayland can't hide one, so the
//! window is a normal process that ends when it is closed). It holds its own single-instance lock, shows the agent's
//! state from a 2 s status poll, and its menu works without the window: Open starts the window process (or brings an
//! open one forward), Open folder, Pause and Resume go to the agent, and Quit stops the agent and ends the window
//! too. The app's start-at-login entry runs this process.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, channel};

use craft_cloud_app::{RunState, Status};

use super::{Clicks, Tray, TrayState, state_of};
use crate::agent::{Agent, ClientAgent, TICK};
use crate::model::Event;
use crate::single_instance::{self, Command, Instance, Name};

/// What the tray process does outside itself.
pub trait Outside {
    /// Brings the window forward, starting its process if none runs.
    fn open_window(&self);
    /// Ends the window's process, if one runs.
    fn end_window(&self);
    fn open_folder(&self, folder: &str);
}

/// The tray process's state: the agent's last status.
pub struct TrayLoop<'a> {
    agent: &'a dyn Agent,
    outside: &'a dyn Outside,
    status: Option<Status>,
}

impl<'a> TrayLoop<'a> {
    pub fn new(agent: &'a dyn Agent, outside: &'a dyn Outside) -> Self {
        Self { agent, outside, status: None }
    }

    /// Asks the agent's status: what the icon shows, and whether Pause reads Resume.
    pub fn poll(&mut self) -> (TrayState, bool) {
        self.status = self.agent.status().ok();
        let paused = self.status.as_ref().is_some_and(|s| s.run_state == RunState::PausedByUser);
        (state_of(self.status.as_ref()), paused)
    }

    /// Carries out a menu click or a command; `false` once the process should end.
    pub fn handle(&mut self, event: Event) -> bool {
        match event {
            Event::Show => self.outside.open_window(),
            Event::OpenFolder => {
                if let Some(folder) = self.status.as_ref().and_then(|s| s.folder.as_deref()) {
                    self.outside.open_folder(folder);
                }
            }
            Event::Pause => {
                let _ = self.agent.pause();
            }
            Event::Resume => {
                let _ = self.agent.resume();
            }
            // Quit stops the sync until the next login: the agent, the window and this process.
            Event::Quit => {
                let _ = self.agent.quit();
                self.outside.end_window();
                return false;
            }
            Event::Exit => return false,
            _ => {}
        }
        true
    }
}

/// The real outside: the window's process through its single-instance channel, and the opener.
struct System {
    dir: std::path::PathBuf,
}

impl Outside for System {
    fn open_window(&self) {
        if single_instance::send(&self.dir, Name::Window, Command::Show).is_err()
            && let Ok(app) = std::env::current_exe()
        {
            let _ = crate::start_agent::spawn_detached(&app, &[]);
        }
    }

    fn end_window(&self) {
        let _ = single_instance::send(&self.dir, Name::Window, Command::Quit);
    }

    fn open_folder(&self, folder: &str) {
        crate::app::open_folder(folder);
    }
}

/// `craft-cloud --tray`: runs until Quit (from its menu, or the window's).
pub fn run() -> ExitCode {
    let Some(dir) = single_instance::folder() else {
        eprintln!("Craft Cloud can't find your user data folder");
        return ExitCode::FAILURE;
    };
    let first = match single_instance::claim(&dir, Name::Tray) {
        Ok(Instance::First(first)) => first,
        // One tray is enough.
        Ok(Instance::Running(_)) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Craft Cloud's tray can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (tx, rx) = channel::<Event>();
    let told = tx.clone();
    first.listen(move |command| {
        let _ = told.send(match command {
            Command::Show => Event::Show,
            Command::Quit => Event::Exit,
        });
    });
    let mut tray = match super::linux::start(Clicks { to: tx, wake: Arc::new(|| {}) }) {
        Ok(Some(tray)) => tray,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Craft Cloud's tray can't start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let agent = ClientAgent::new(None);
    let outside = System { dir };
    let mut tray_loop = TrayLoop::new(&agent, &outside);
    loop {
        let (state, paused) = tray_loop.poll();
        tray.show(state, paused);
        match rx.recv_timeout(TICK) {
            Ok(event) => {
                if !tray_loop.handle(event) {
                    return ExitCode::SUCCESS;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return ExitCode::SUCCESS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::fake::FakeAgent;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Recorded(Mutex<Vec<String>>);

    impl Outside for Recorded {
        fn open_window(&self) {
            self.0.lock().unwrap().push("open window".into());
        }
        fn end_window(&self) {
            self.0.lock().unwrap().push("end window".into());
        }
        fn open_folder(&self, folder: &str) {
            self.0.lock().unwrap().push(format!("open {folder}"));
        }
    }

    /// The tray process's menu works without a window: Open starts or raises it, Open folder and Pause reach their
    /// targets, Quit stops the agent and ends the window and the tray, and the window's Quit ends the tray.
    #[test]
    fn the_tray_process_menu_works_without_the_window() {
        let agent = FakeAgent::new(crate::model::tests::status(true));
        let outside = Recorded::default();
        let mut tray = TrayLoop::new(&agent, &outside);
        assert_eq!(tray.poll(), (TrayState::Synced, false));
        assert!(tray.handle(Event::Show));
        assert!(tray.handle(Event::OpenFolder));
        assert!(tray.handle(Event::Pause));
        assert!(!tray.handle(Event::Quit), "Quit ends the tray process");
        assert_eq!(outside.0.lock().unwrap().as_slice(), ["open window", "open /home/ana/Craft Cloud", "end window"]);
        let asked = agent.asked.lock().unwrap().clone();
        assert_eq!(asked, ["status", "pause", "quit"]);
        assert!(!TrayLoop::new(&agent, &outside).handle(Event::Exit), "the window's Quit ends it too");
    }

    #[test]
    fn no_agent_needs_attention() {
        let agent = FakeAgent::new(crate::model::tests::status(true));
        agent.gone.store(true, std::sync::atomic::Ordering::SeqCst);
        let outside = Recorded::default();
        assert_eq!(TrayLoop::new(&agent, &outside).poll(), (TrayState::NeedsAttention, false));
    }
}
