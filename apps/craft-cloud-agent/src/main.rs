#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;

use clap::Parser;
use craft_cloud_agent::cli::{Cli, Exit, run};

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
    let exit = match run(cli, &mut std::io::stdin().lock(), &mut std::io::stdout()).await {
        Ok(exit) => exit,
        Err(e) => {
            eprintln!("error: {e}");
            e.exit()
        }
    };
    ExitCode::from(exit.code())
}
