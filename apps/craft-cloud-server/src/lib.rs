//! The Craft Cloud server: an HTTP API over the metadata store (`craft-cloud-meta`) and the blob store
//! (`craft-cloud-store`), the live service on a WebSocket ([`live`]), plus the admin command line.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod api;
pub mod auth;
pub mod cli;
pub mod config;
pub mod error;
pub mod join;
pub mod live;
pub mod notify;
pub mod ratelimit;
pub mod state;

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;

pub use state::AppState;

/// Serves the API on `listener` until `shutdown` resolves.
pub async fn serve(state: AppState, listener: TcpListener, shutdown: impl std::future::Future<Output = ()> + Send + 'static) -> std::io::Result<()> {
    let state = Arc::new(state);
    let app = api::router(state.clone());
    // Long-polls are told to answer before axum starts waiting for open requests to finish.
    let shutdown = async move {
        shutdown.await;
        state.notifier.begin_shutdown();
    };
    // No Nagle delay: a live update is a few dozen bytes, and must go out at once (a keystroke reaches the other
    // machines in well under 250 ms).
    use axum::serve::ListenerExt;
    let listener = listener.tap_io(|tcp| {
        let _ = tcp.set_nodelay(true);
    });
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown).await
}
