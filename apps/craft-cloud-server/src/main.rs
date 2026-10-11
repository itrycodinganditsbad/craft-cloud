#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;

use clap::Parser;
use craft_cloud_server::cli::{Cli, Command, open};
use craft_cloud_server::config::Config;
use craft_cloud_server::{AppState, serve};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve { config } => run_server(&config).await,
        command => craft_cloud_server::cli::run(command, &mut std::io::stdin().lock(), &mut std::io::stdout()).await.map_err(|e| e.to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run_server(config_path: &std::path::Path) -> Result<(), String> {
    let config = Config::load(config_path).map_err(|e| e.to_string())?;
    if config.behind_proxy && config.trusted_proxies.is_empty() {
        tracing::warn!("behind_proxy is set but trusted_proxies is empty: every client will share the proxy's address for sign-in rate limits");
    }
    let (meta, store) = open(&config).await.map_err(|e| e.to_string())?;
    let state = AppState::new(meta, store, config.trusted_proxies.clone()).map_err(|e| e.message)?;
    let listener = tokio::net::TcpListener::bind(config.listen).await.map_err(|e| format!("can't listen on {}: {e}", config.listen))?;
    tracing::info!(listen = %config.listen, behind_proxy = config.behind_proxy, "Craft Cloud server running");
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutting down");
    };
    serve(state, listener, shutdown).await.map_err(|e| e.to_string())
}
