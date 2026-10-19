//! `craft-cloud-setup.exe [--no-start-menu] [--quiet]` installs Craft Cloud for this user; `--uninstall [--quiet]`
//! removes it. Without `--quiet` it ends with a question ("Open Craft Cloud now?") or a message; with it, only the
//! exit code says how it went (0: done, 1: failed).
#![cfg_attr(windows, windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;

/// What the command line asks.
struct Args {
    uninstall: bool,
    quiet: bool,
    start_menu: bool,
}

fn args() -> Result<Args, String> {
    let mut args = Args { uninstall: false, quiet: false, start_menu: true };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--uninstall" => args.uninstall = true,
            "--quiet" => args.quiet = true,
            "--no-start-menu" => args.start_menu = false,
            other => return Err(format!("unknown option {other}: use --no-start-menu, --quiet or --uninstall")),
        }
    }
    Ok(args)
}

/// Says `text`, or asks it as a yes/no question (`ask`): whether the answer was yes.
#[cfg(not(windows))]
fn tell(text: &str, _ask: bool, _error: bool) -> bool {
    eprintln!("{text}");
    false
}

/// Says `text`, or asks it as a yes/no question (`ask`): whether the answer was yes.
#[cfg(windows)]
fn tell(text: &str, ask: bool, error: bool) -> bool {
    let level = if error { rfd::MessageLevel::Error } else { rfd::MessageLevel::Info };
    let buttons = if ask { rfd::MessageButtons::YesNo } else { rfd::MessageButtons::Ok };
    let answer = rfd::MessageDialog::new().set_title("Craft Cloud").set_description(text).set_level(level).set_buttons(buttons).show();
    answer == rfd::MessageDialogResult::Yes
}

fn main() -> ExitCode {
    let args = match args() {
        Ok(args) => args,
        Err(e) => {
            tell(&e, false, true);
            return ExitCode::FAILURE;
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if !args.quiet {
                tell(&e, false, true);
            }
            ExitCode::FAILURE
        }
    }
}

#[cfg(windows)]
fn run(args: &Args) -> Result<(), String> {
    use craft_cloud_setup::windows::{ThisMachine, UserRegistry};
    use craft_cloud_setup::{APP_EXE, Options, PAYLOAD, Places, install, uninstall};

    let places = Places::for_user()?;
    let this = std::env::current_exe().map_err(|e| format!("the setup can't tell where it is: {e}"))?;
    if args.uninstall {
        uninstall(&places, &UserRegistry, &ThisMachine, &this)?;
        if !args.quiet {
            tell("Craft Cloud is removed from this computer. Your synced folder and its files stay, and so does the sign-in.", false, false);
        }
        return Ok(());
    }
    let options = Options { start_menu: args.start_menu, version: env!("CARGO_PKG_VERSION").to_owned() };
    let installed = install(&places, &UserRegistry, &ThisMachine, PAYLOAD, &this, &options)?;
    if args.quiet {
        return Ok(());
    }
    let mut text = format!("Craft Cloud {} is installed in {}.", options.version, installed.dir.display());
    for warning in &installed.warnings {
        text.push_str(&format!("\n\nBut: {warning}."));
    }
    text.push_str("\n\nOpen Craft Cloud now?");
    if tell(&text, true, false) {
        let app = installed.dir.join(APP_EXE);
        std::process::Command::new(&app).spawn().map_err(|e| format!("{}: {e}", app.display()))?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn run(_args: &Args) -> Result<(), String> {
    Err("craft-cloud-setup is Windows' installer; on this system use the package for it (see docs/install.md)".into())
}
