#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use craft_cloud_agent::cli::{Cli, Command, Exit, run};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // A usage error is a plain failure (1), never the "deletions wait" code 2 that clap's own exit uses;
            // `--help` and `--version` are not errors.
            let failed = e.use_stderr();
            let _ = e.print();
            return ExitCode::from(if failed { Exit::Failed } else { Exit::Done }.code());
        }
    };
    let looping = matches!(cli.command, Command::Run);
    let exit = match run(cli, &mut std::io::stdin().lock(), &mut std::io::stdout()).await {
        Ok(exit) => exit,
        Err(e) => {
            eprintln!("error: {e}");
            e.exit()
        }
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
