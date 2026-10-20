//! The live hub: the agent's side of live documents: each app's recents, shared by every machine of the account, and
//! documents of files, opened by apps by path.
//!
//! - [`blobs`]: snapshots up and down through the chunk API.
//! - [`copy`]: a document's local copy (a Loro document merged from opaque updates, checked by Loro) and what the
//!   server's log holds of it.
//! - [`recents`]: the recents document and the list apps see.
//! - [`replica`]: the protocol state for one account: documents, what it has of each server log, unacked updates.
//! - [`store`]: where documents are kept between runs (`<data folder>/live/<space>/`, one file each, atomic; a journal
//!   beside each document of a file).
//! - [`link`]: the WebSocket to the server's live service.
//! - [`hub`]: the task that ties them together while the agent is signed in, and serves the local API.
//! - [`docs`]: the local API's documents WebSocket.
//!
//! The local API's recents WebSocket ([`recents_socket`]) is a client of the hub like any other: an app subscribes
//! to its list (the whole list comes now and after every change) and adds entries (answered once they are on disk).

pub mod app_blobs;
pub mod blobs;
pub mod copy;
pub mod docs;
pub mod hub;
pub mod link;
pub mod recents;
pub mod replica;
pub mod saves;
pub mod store;

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use craft_cloud_proto::api::codes as api_codes;
use craft_cloud_proto::live::AppName;
use craft_cloud_proto::local::{MAX_LOCAL_JSON_BYTES, RecentsEvent, RecentsRequest, codes};
use tokio::sync::mpsc;

pub use hub::{Hub, HubConfig, HubError, HubHandle};

use crate::local_api::{ApiState, LocalError};

/// Lists one recents connection may have queued for its app (the followers wait while it is full; a list that
/// changes again meanwhile is sent once, as it is then).
const QUEUE: usize = 64;
/// A message the client doesn't take within this long drops it. Each request is answered before the next one is read,
/// so a client that sends without reading is held up by its own answers, and then dropped.
const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `GET /v1/recents`: the WebSocket of an app's recents. Signed out there is nothing to follow: `409 not_signed_in`.
pub async fn recents_socket(State(api): State<Arc<ApiState>>, upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>) -> Response {
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(e) => return LocalError::new(e.status(), api_codes::INVALID, e.body_text()).into_response(),
    };
    let Some(hub) = api.control.hub() else {
        return LocalError::new(StatusCode::CONFLICT, codes::NOT_SIGNED_IN, "not signed in: sign in first").into_response();
    };
    upgrade.max_message_size(MAX_LOCAL_JSON_BYTES).max_frame_size(MAX_LOCAL_JSON_BYTES).on_upgrade(move |socket| serve(socket, hub))
}

fn error(id: Option<u64>, e: &HubError) -> RecentsEvent {
    let code = match e {
        HubError::Invalid(_) => api_codes::INVALID,
        HubError::Failed(_) => api_codes::INTERNAL,
        HubError::Gone => codes::NOT_SIGNED_IN,
    };
    RecentsEvent::Error { id, code: code.into(), message: e.to_string() }
}

/// One app's connection: requests in, lists and answers out, until either side goes (or the hub stops: signed out).
async fn serve(mut socket: WebSocket, hub: HubHandle) {
    let (out, mut queue) = mpsc::channel::<Option<RecentsEvent>>(QUEUE);
    loop {
        tokio::select! {
            message = socket.recv() => {
                let request = match message {
                    Some(Ok(Message::Text(text))) => serde_json::from_str::<RecentsRequest>(&text),
                    Some(Ok(Message::Binary(bytes))) => serde_json::from_slice::<RecentsRequest>(&bytes),
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                };
                let event = match request {
                    Err(e) => Some(RecentsEvent::Error { id: None, code: api_codes::INVALID.into(), message: format!("not a recents request: {e}") }),
                    Ok(RecentsRequest::Subscribe { app }) => match AppName::new(&app) {
                        Err(e) => Some(RecentsEvent::Error { id: None, code: api_codes::INVALID.into(), message: e.to_string() }),
                        Ok(name) => match hub.follow(name).await {
                            Err(e) => Some(error(None, &e)),
                            Ok(mut list) => {
                                let out = out.clone();
                                // The list now, then after every change; `None` once the hub is gone.
                                tokio::spawn(async move {
                                    loop {
                                        let listing = list.borrow_and_update().clone();
                                        let event = RecentsEvent::Entries { app: app.clone(), entries: listing.entries, push_stopped: listing.push_stopped };
                                        if out.send(Some(event)).await.is_err() {
                                            return;
                                        }
                                        if list.changed().await.is_err() {
                                            let _ = out.send(None).await;
                                            return;
                                        }
                                    }
                                });
                                None
                            }
                        },
                    },
                    Ok(RecentsRequest::Add { id, app, entry }) => match AppName::new(&app) {
                        Err(e) => Some(RecentsEvent::Error { id: Some(id), code: api_codes::INVALID.into(), message: e.to_string() }),
                        Ok(name) => Some(match hub.add(name, entry).await {
                            Ok(entry) => RecentsEvent::Added { id, entry },
                            Err(e) => error(Some(id), &e),
                        }),
                    },
                };
                // Sent here, before the next request is read: never put on the queue only this loop drains (it would
                // wait for itself). A client that doesn't take it within the send timeout is dropped.
                if let Some(event) = event
                    && !send_event(&mut socket, &event).await
                {
                    break;
                }
            }
            event = queue.recv() => match event {
                Some(Some(event)) => {
                    if !send_event(&mut socket, &event).await {
                        break;
                    }
                }
                // The hub stopped (signed out): the app is told, and the connection ends.
                Some(None) | None => {
                    let _ = send_event(&mut socket, &error(None, &HubError::Gone)).await;
                    break;
                }
            },
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

/// Sends one event within [`SEND_TIMEOUT`]; false when the connection is done (or the client too slow to read).
async fn send_event(socket: &mut WebSocket, event: &RecentsEvent) -> bool {
    let Ok(text) = serde_json::to_string(event) else { return true };
    matches!(tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Text(text.into()))).await, Ok(Ok(())))
}
