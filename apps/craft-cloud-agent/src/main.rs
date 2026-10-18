#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::Write;
use std::process::ExitCode;

use craft_cloud_agent::cli::{Exit, main as agent_main};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let (exit, looping) =
        agent_main(std::env::args_os().collect(), &mut std::io::stdin().lock(), &mut std::io::stdout(), &mut std::io::stderr()).await;
    // A loop that ended at once (a second Ctrl-C) may leave a pass or a name lookup running: the process ends now
    // instead of waiting for them while the runtime shuts down. The loop already wrote its status, and the pass's
    // checkpoints keep a cut-short pass safe, as after a crash.
    if looping && exit == Exit::Failed {
        let _ = std::io::stdout().flush();
        std::process::exit(i32::from(exit.code()));
    }
    ExitCode::from(exit.code())
}
