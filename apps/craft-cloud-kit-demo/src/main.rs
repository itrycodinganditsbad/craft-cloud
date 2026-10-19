//! `craft-cloud-kit-demo <file>`: opens `<file>` (relative to the synced folder, or absolute inside it; a new file if
//! there is none) in the demo's window, live through the Craft Cloud agent running on this machine.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use craft_cloud_kit::Agent;
use craft_cloud_kit_demo::ui::Demo;

struct App(Demo);

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.0.show(ui);
    }

    fn on_exit(&mut self) {
        // What wasn't saved yet goes to the file (the live document has it either way).
        self.0.save();
    }
}

fn main() -> ExitCode {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: craft-cloud-kit-demo <file>  (relative to the Craft Cloud folder, e.g. Notes/demo.kitdemo)");
        return ExitCode::from(2);
    };
    let client = craft_cloud_app::Client::connect();
    let status = client.as_ref().ok().and_then(|c| c.status().ok());
    let folder: Option<PathBuf> = status.as_ref().and_then(|s| s.folder.clone()).map(PathBuf::from);
    let machine = status.as_ref().and_then(|s| s.machine.clone()).unwrap_or_else(|| "this computer".into());
    // The file as the agent knows it: relative to the synced folder, `/`-separated.
    let (folder, rel) = match &folder {
        Some(f) => match Path::new(&arg).strip_prefix(f) {
            Ok(inside) => (f.clone(), inside.to_string_lossy().replace('\\', "/")),
            Err(_) if Path::new(&arg).is_absolute() => {
                eprintln!("{arg} isn't inside the Craft Cloud folder {}: it opens as a plain file", f.display());
                (Path::new(&arg).parent().map(Path::to_path_buf).unwrap_or_default(), file_name(&arg))
            }
            Err(_) => (f.clone(), arg.replace('\\', "/")),
        },
        None => {
            let path = PathBuf::from(&arg);
            (path.parent().map(Path::to_path_buf).unwrap_or_default(), file_name(&arg))
        }
    };
    let agent = client.is_ok().then(Agent::local);
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 700.0]), ..Default::default() };
    let title = format!("Craft Cloud kit demo: {rel}");
    let result = eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            Ok(Box::new(App(Demo::open(agent, &folder, &rel, &machine, move || ctx.request_repaint()))))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("the window failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn file_name(arg: &str) -> String {
    Path::new(arg).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| arg.to_owned())
}
