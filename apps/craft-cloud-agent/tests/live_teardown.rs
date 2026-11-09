//! An app that quits right after sending its last changes loses none of them: the agent takes in every frame the app
//! sent before its close, even though the app's socket is already gone (its acknowledgements have nowhere to go) or a
//! write to it failed while its changes were still arriving.

mod looping;

use std::sync::atomic::Ordering;
use std::time::Duration;

use craft_cloud_agent::live::docs::{FAIL_WRITES, FAIL_WRITES_OF, READ_ON_MS};
use craft_cloud_app::live::{Event, Opened};
use craft_cloud_proto::local::docs::{STATE, UPDATE, decode_binary, encode_binary};
use futures_util::{SinkExt, StreamExt};
use looping::{Agent, Server, eventually};
use loro::{ExportMode, LoroDoc};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// How many one-letter changes the app sends before it quits.
const CHANGES: usize = 60;

/// The two tests use the process-wide `FAIL_WRITES` hook, so they run one after the other.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Clears the hook when the test ends, however it ends.
struct Healthy;

impl Drop for Healthy {
    fn drop(&mut self) {
        FAIL_WRITES.store(false, Ordering::Relaxed);
        READ_ON_MS.store(0, Ordering::Relaxed);
        if let Ok(mut named) = FAIL_WRITES_OF.lock() {
            *named = None;
        }
    }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn off<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

async fn connect(agent: &Agent) -> Ws {
    let api: craft_cloud_proto::local::LocalApiFile =
        serde_json::from_slice(&std::fs::read(agent.data.path().join(craft_cloud_proto::local::LOCAL_API_FILE)).unwrap()).unwrap();
    let mut request = format!("ws://127.0.0.1:{}/v1/docs", api.port).into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {}", api.token).parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

/// The next message, within 10 s.
async fn next(ws: &mut Ws) -> Message {
    tokio::time::timeout(Duration::from_secs(10), ws.next()).await.expect("the agent answers").expect("the connection is open").unwrap()
}

/// Makes `notes.txt` live on `pc` (an app imports it), after `laptop` has the file too. The returned documents stay
/// open for the test's length.
async fn live_file(pc: &Agent, laptop: &Agent) -> craft_cloud_app::live::Documents {
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("notes.txt"), "start").unwrap();
    let file = laptop.folder.join("notes.txt");
    eventually("notes.txt reaches the laptop", move || file.exists().then_some(())).await;
    let client = pc.client().await;
    off(move || {
        let docs = client.documents().unwrap();
        match docs.open("notes.txt", "wordcraft").unwrap() {
            Opened::NeedsImport(importer) => {
                importer.import(&craft_cloud_agent::doc::plain_text_snapshot("start").unwrap()).unwrap();
            }
            _ => panic!("the file was never live"),
        }
        docs
    })
    .await
}

/// An app that opens `notes.txt`, sends [`CHANGES`] changes, then closes the document and its socket at once, as an
/// app does when it quits. With `fail_writes`, the agent's writes to the app fail meanwhile. The text it ends with.
async fn quit_right_after_typing(pc: &Agent, fail_writes: bool) -> String {
    let mut ws = connect(pc).await;
    let open = serde_json::json!({ "type": "open", "id": 1, "path": "notes.txt", "app": "wordcraft" });
    ws.send(Message::Text(open.to_string().into())).await.unwrap();
    let opened = loop {
        if let Message::Text(text) = next(&mut ws).await {
            let event: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
            if event["type"] == "opened" {
                break event;
            }
        }
    };
    // The state follows as a binary frame.
    let loro = LoroDoc::new();
    loro.set_peer_id(77).unwrap();
    loop {
        if let Message::Binary(bytes) = next(&mut ws).await {
            let (kind, _, body) = decode_binary(&bytes).unwrap();
            if kind == STATE {
                loro.import(body).unwrap();
                break;
            }
        }
    }
    FAIL_WRITES.store(fail_writes, Ordering::Relaxed);
    if fail_writes {
        // An update the agent refuses, and cannot say so: a write to the app fails before it has read the rest.
        let too_big = vec![0u8; craft_cloud_proto::live::MAX_UPDATE_BYTES + 1];
        let _ = ws.send(Message::Binary(encode_binary(UPDATE, 1, &too_big).into())).await;
    }
    let text = loro.get_text("text");
    for i in 0..CHANGES {
        let before = loro.oplog_vv();
        text.insert(text.len_unicode(), &format!("{}", i % 10)).unwrap();
        loro.commit();
        let update = loro.export(ExportMode::updates(&before)).unwrap();
        // An agent that has dropped the connection refuses the rest; what it took in is what the checks below see.
        let _ = ws.send(Message::Binary(encode_binary(UPDATE, 1, &update).into())).await;
    }
    let close = serde_json::json!({ "type": "close", "doc": opened["doc"] });
    let _ = ws.send(Message::Text(close.to_string().into())).await;
    let _ = ws.send(Message::Close(None)).await;
    // Gone at once, as the app's teardown does: the agent's answers have nowhere to go.
    drop(ws);
    text.to_string()
}

/// Waits for `laptop` to open `notes.txt` with exactly `expected`: every change reached the pc's journal and went up.
async fn the_laptop_gets(laptop: &Agent, expected: String) {
    let client = laptop.client().await;
    eventually("the laptop shows every change of the app that quit", move || {
        let client = client.clone();
        let expected = expected.clone();
        std::thread::spawn(move || {
            let docs = client.documents().ok()?;
            let Opened::Live(doc) = docs.open("notes.txt", "wordcraft").ok()? else { return None };
            let loro = LoroDoc::new();
            loro.import(doc.state()).ok()?;
            for update in doc.state_updates() {
                loro.import(update).ok()?;
            }
            while let Some(event) = doc.try_recv() {
                if let Event::Update(bytes) = event {
                    loro.import(&bytes).ok()?;
                }
            }
            (loro.get_text("text").to_string() == expected).then_some(())
        })
        .join()
        .ok()
        .flatten()
    })
    .await;
}

/// The updates, the document's close and the socket's end are all queued before the agent has handled the first; the
/// first one's acknowledgement has nowhere to go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_that_closes_its_socket_right_after_its_changes_loses_none() {
    let _one = ONE_AT_A_TIME.lock().await;
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let _docs = live_file(&pc, &laptop).await;
    let typed = quit_right_after_typing(&pc, false).await;
    assert_eq!(typed.len(), "start".len() + CHANGES);
    the_laptop_gets(&laptop, typed).await;
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// A write to the app fails (the answer to a refused update) while its changes are still arriving: the agent stops
/// writing and reads on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_write_to_the_app_does_not_cut_off_the_changes_still_arriving() {
    let _one = ONE_AT_A_TIME.lock().await;
    let _healthy = Healthy;
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let _docs = live_file(&pc, &laptop).await;
    let typed = quit_right_after_typing(&pc, true).await;
    FAIL_WRITES.store(false, Ordering::Relaxed);
    assert_eq!(typed.len(), "start".len() + CHANGES);
    the_laptop_gets(&laptop, typed).await;
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// Two apps wait for the import of one file; the one asked first stops taking messages (its connection stays open).
/// The other is asked at once, not when the first connection is finally cleaned up.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_app_is_asked_for_the_import_when_the_first_one_stops_taking_messages() {
    let _one = ONE_AT_A_TIME.lock().await;
    let _healthy = Healthy;
    let server = Server::new().await;
    let pc = Agent::signed_in(&server, "pc").await;
    let run_pc = pc.run();
    // The local API is up once a client connects.
    let _client = pc.client().await;
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("notes.txt"), "start").unwrap();
    // The first connection is read on for far longer than this test lasts, so its `Gone` cannot be what helps.
    READ_ON_MS.store(120_000, Ordering::Relaxed);
    let open = serde_json::json!({ "type": "open", "id": 1, "path": "notes.txt", "app": "stuck" });
    let mut first = connect(&pc).await;
    first.send(Message::Text(open.to_string().into())).await.unwrap();
    loop {
        if let Message::Text(text) = next(&mut first).await {
            let event: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
            if event["type"] == "needs_import" {
                break;
            }
        }
    }
    // From now on, writes to the first app fail.
    *FAIL_WRITES_OF.lock().unwrap() = Some("stuck".into());
    let mut second = connect(&pc).await;
    let open = serde_json::json!({ "type": "open", "id": 1, "path": "notes.txt", "app": "wordcraft" });
    second.send(Message::Text(open.to_string().into())).await.unwrap();
    // The first app's updates for its still unanswered open, which the hub can only refuse, make it find out that the first
    // app takes nothing.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let asked = loop {
        assert!(std::time::Instant::now() < deadline, "the second app was never asked for the import");
        let _ = first.send(Message::Binary(encode_binary(UPDATE, 1, b"x").into())).await;
        if let Ok(Some(Ok(Message::Text(text)))) = tokio::time::timeout(Duration::from_millis(100), second.next()).await {
            let event: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
            if event["type"] == "needs_import" {
                break event;
            }
        }
    };
    assert_eq!(asked["id"], 1);
    drop((first, second));
    run_pc.stop().await.0.unwrap();
}
