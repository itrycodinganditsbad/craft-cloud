#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::Write;
use std::process::ExitCode;

use craft_cloud_agent::cli::{Exit, log_for, main as agent_main};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // `run --log` (what start at login runs) writes to `run.log` in the data folder instead of the console.
    let (exit, looping) = match log_for(&args, false) {
        Some(log) => {
            let (mut out, mut err) = (log.clone(), log);
            agent_main(args, &mut std::io::empty(), &mut out, &mut err).await
        }
        None => agent_main(args, &mut std::io::stdin().lock(), &mut std::io::stdout(), &mut std::io::stderr()).await,
    };
    // A loop that ended at once (a second Ctrl-C) may leave a pass or a name lookup running: the process ends now
    // instead of waiting for them while the runtime shuts down. The loop already wrote its status, and the pass's
    // checkpoints keep a cut-short pass safe, as after a crash.
    if looping && exit == Exit::Failed {
        let _ = std::io::stdout().flush();
        std::process::exit(i32::from(exit.code()));
    }
    ExitCode::from(exit.code())
}
