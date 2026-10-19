//! `craft-cloud`: the Craft Cloud app. A second start brings the first one's window forward and exits.
//!
//! - Windows and macOS: the window and the tray icon are one process; `--hidden` starts it in the tray, as its
//!   start-at-login entry does, and closing the window hides it there.
//! - Linux: `--tray` is the tray process (no window: [`tray::process`]), which the start-at-login entry runs (an
//!   older entry's `--hidden` means the same). The window is a process of its own that ends when closed (Wayland can't
//!   hide a window), and starts the tray process if none runs.
//!
//! `--close` ends the window and the tray process that run (an installer's upgrade: also an older version's) and
//! prints which did (`window`, `tray`, a line each); the agent is left alone.
#![cfg_attr(windows, windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use craft_cloud_desktop::agent::ClientAgent;
use craft_cloud_desktop::app::{App, System};
use craft_cloud_desktop::model::{Event, Model};
use craft_cloud_desktop::single_instance::{self, Command, Instance, Name};
use craft_cloud_desktop::{tray, ui};

fn main() -> ExitCode {
    let has = |flag: &str| std::env::args_os().skip(1).any(|arg| arg == flag);
    let linux = cfg!(target_os = "linux");
    #[cfg(target_os = "linux")]
    if has("--tray") || has("--hidden") {
        return tray::process::run();
    }
    let hidden = !linux && has("--hidden");
    let Some(dir) = single_instance::folder() else {
        eprintln!("Craft Cloud can't find your user data folder");
        return ExitCode::FAILURE;
    };
    if has("--close") {
        for name in single_instance::close_running(&dir) {
            println!("{}", if name == Name::Tray { "tray" } else { "window" });
        }
        return ExitCode::SUCCESS;
    }
    let (outside_tx, outside_rx) = std::sync::mpsc::channel::<Event>();
    let window: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    match single_instance::claim(&dir, Name::Window) {
        Ok(Instance::Running(running)) => {
            return match running.show() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Craft Cloud is open already, but doesn't answer: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(Instance::First(first)) => {
            let (tx, window) = (outside_tx.clone(), window.clone());
            first.listen(move |command| {
                let _ = tx.send(match command {
                    Command::Show => Event::Show,
                    // The tray process quit Craft Cloud.
                    Command::Quit => Event::Exit,
                });
                if let Some(ctx) = window.get() {
                    ctx.request_repaint();
                }
            });
        }
        Err(e) => {
            eprintln!("Craft Cloud can't start: {e}");
            return ExitCode::FAILURE;
        }
    }
    // Linux: the tray is its own process; the window starts it if none runs.
    if linux
        && !single_instance::is_running(&dir, Name::Tray)
        && let Ok(app) = std::env::current_exe()
    {
        let _ = craft_cloud_desktop::start_agent::spawn_detached(&app, &["--tray"]);
    }

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(ui::TITLE)
        .with_app_id("craft-cloud")
        .with_inner_size(ui::WINDOW_SIZE)
        .with_min_inner_size([360.0, 420.0])
        .with_visible(!hidden);
    if let Ok((width, height, rgba)) = tray::rgba(tray::APP_ICON) {
        viewport = viewport.with_icon(egui::IconData { rgba, width, height });
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let started = eframe::run_native(
        ui::TITLE,
        options,
        Box::new(move |cc| {
            let _ = window.set(cc.egui_ctx.clone());
            // The tray is made on this (the event loop's) thread, as Windows and macOS need.
            let wake_ctx = cc.egui_ctx.clone();
            let clicks = tray::Clicks { to: outside_tx.clone(), wake: Arc::new(move || wake_ctx.request_repaint()) };
            let tray = tray::start(clicks);
            let mut model = Model::new(craft_cloud_desktop::join_defaults(), tray.is_some(), craft_cloud_desktop::agent_log());
            // Linux: the login entry is the tray process's, kept whether or not a tray is there now.
            model.login_entry = tray.is_some() || linux;
            // Started hidden with no tray to live in (an entry left over from a desktop that had one): shown after all.
            model.window_visible = !hidden || tray.is_none();
            let agent = Arc::new(ClientAgent::new(None));
            let host = Box::new(System { tray, tray_process: linux.then_some(dir) });
            let app = App::new(&cc.egui_ctx, agent, craft_cloud_desktop::start_agent::starter(), model, (outside_tx, outside_rx), host);
            Ok(Box::new(app))
        }),
    );
    match started {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Craft Cloud's window can't open: {e}");
            ExitCode::FAILURE
        }
    }
}
