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
use craft_cloud_desktop::ui;

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

    let mut model = Model::new(craft_cloud_desktop::join_defaults(), false, craft_cloud_desktop::agent_log());
    model.window_visible = !hidden;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(ui::TITLE)
            .with_inner_size(ui::WINDOW_SIZE)
            .with_min_inner_size([360.0, 420.0])
            .with_visible(!hidden),
        ..Default::default()
    };
    let started = eframe::run_native(
        ui::TITLE,
        options,
        Box::new(move |cc| {
            let _ = window.set(cc.egui_ctx.clone());
            let agent = Arc::new(ClientAgent::new(None));
            let app = App::new(&cc.egui_ctx, agent, craft_cloud_desktop::start_agent::starter(), model, (outside_tx, outside_rx), Box::new(System));
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
