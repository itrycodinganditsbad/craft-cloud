//! `craft-cloud`: the Craft Cloud app.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;
use std::sync::Arc;

use craft_cloud_desktop::agent::{ClientAgent, Link};
use craft_cloud_desktop::model::{Effect, Event, Model, Screen};
use craft_cloud_desktop::single_instance::{self, Instance};

fn main() -> ExitCode {
    let Some(dir) = single_instance::folder() else {
        eprintln!("Craft Cloud can't find your user data folder");
        return ExitCode::FAILURE;
    };
    match single_instance::claim(&dir) {
        Ok(Instance::Running(running)) => {
            return match running.show() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Craft Cloud is already open, but doesn't answer: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Ok(Instance::First(first)) => first.listen(|| {}),
        Err(e) => {
            eprintln!("Craft Cloud can't start: {e}");
            return ExitCode::FAILURE;
        }
    }
    // Without a window yet: find or start the agent, and say what the window would show.
    let link = Link::new(Arc::new(ClientAgent::new(None)), craft_cloud_desktop::start_agent::starter(), Arc::new(|| {}));
    let mut model = Model::new(craft_cloud_desktop::join_defaults(), false, craft_cloud_desktop::agent_log());
    let mut pending = model.update(Event::Start);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while matches!(model.screen, Screen::Starting) && std::time::Instant::now() < deadline {
        for effect in pending.drain(..) {
            if let Some(Effect::QuitApp) = link.perform(effect) {
                return ExitCode::SUCCESS;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        for event in link.events() {
            pending.extend(model.update(event));
        }
    }
    println!("{:?}", model.screen);
    ExitCode::SUCCESS
}
