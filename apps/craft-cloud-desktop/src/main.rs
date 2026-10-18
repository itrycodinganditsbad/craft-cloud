//! `craft-cloud`: the Craft Cloud app. `--hidden` starts it without its window (in the tray), as its start-at-login
//! entry does. A second start brings the first one's window forward and exits.
#![cfg_attr(windows, windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;
use std::sync::{Arc, OnceLock};

use craft_cloud_desktop::agent::ClientAgent;
use craft_cloud_desktop::app::{App, System};
use craft_cloud_desktop::model::{Event, Model};
use craft_cloud_desktop::single_instance::{self, Instance};
use craft_cloud_desktop::{tray, ui};

fn main() -> ExitCode {
    let hidden = std::env::args_os().skip(1).any(|arg| arg == "--hidden");
    let Some(dir) = single_instance::folder() else {
        eprintln!("Craft Cloud can't find your user data folder");
        return ExitCode::FAILURE;
    };
    let (outside_tx, outside_rx) = std::sync::mpsc::channel::<Event>();
    let window: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    match single_instance::claim(&dir) {
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
            first.listen(move || {
                let _ = tx.send(Event::Show);
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
            // Started hidden with no tray to live in (an entry left over from a desktop that had one): shown after all.
            model.window_visible = !hidden || tray.is_none();
            let agent = Arc::new(ClientAgent::new(None));
            let host = Box::new(System { tray });
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
