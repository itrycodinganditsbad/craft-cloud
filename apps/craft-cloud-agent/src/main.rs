#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;

use clap::Parser;
use craft_cloud_agent::cli::{Cli, run};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let exit = match run(cli, &mut std::io::stdin().lock(), &mut std::io::stdout()).await {
        Ok(exit) => exit,
        Err(e) => {
            eprintln!("error: {e}");
            e.exit()
        }
    };
    ExitCode::from(exit.code())
}
