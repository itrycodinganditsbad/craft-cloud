//! `craft-cloud-agent-bg`: the agent built without a console window on Windows (the GUI subsystem), for starting at
//! login (`craft-cloud-agent-bg run`). It runs the same `main` as `craft-cloud-agent`, and writes what that prints to
//! `run.log` in the data folder (rotated at 10 MiB, one older `run.log.1` kept), since there is no console to print
//! to. Built on every OS; only Windows needs it.
#![cfg_attr(windows, windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use craft_cloud_agent::account::DataDir;
use craft_cloud_agent::cli::{Cli, Exit, main as agent_main};
use craft_cloud_agent::runlog::{LOG_FILE, MAX_LOG_BYTES, RunLog};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // The log goes in the data folder the command line names (or the default one). Without one, or if it can't be
    // opened, the output goes nowhere: the agent still runs.
    let flag = Cli::try_parse_from(&args).ok().and_then(|cli| cli.data_dir);
    let log = DataDir::resolve(flag).ok().and_then(|data| {
        data.create().ok()?;
        RunLog::open(&data.path().join(LOG_FILE), MAX_LOG_BYTES).ok()
    });
    let (exit, looping) = match log {
        Some(log) => {
            let (mut out, mut err) = (log.clone(), log);
            agent_main(args, &mut std::io::empty(), &mut out, &mut err).await
        }
        None => agent_main(args, &mut std::io::empty(), &mut std::io::sink(), &mut std::io::sink()).await,
    };
    // As in the console build: after a second stop, the process ends now.
    if looping && exit == Exit::Failed {
        let _ = std::io::stdout().flush();
        std::process::exit(i32::from(exit.code()));
    }
    ExitCode::from(exit.code())
}
