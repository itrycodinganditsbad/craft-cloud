//! Live documents end to end: running agents of one account against a real server, the files synced by the file sync,
//! driven through `craft-cloud-app`'s live documents as an app would. A file opened on one machine is live on the
//! other; two machines importing a never-live file at once end on one document; a file renamed, moved or deleted on
//! another machine keeps its open document (and says when it was deleted); and an app that misbehaves on the local
//! API is refused or dropped while the agent and other apps go on.

mod looping;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use craft_cloud_app::live::{Document, Documents, Event, NotLiveReason, Opened};
use looping::{Agent, Server, eventually};
use loro::{ExportMode, LoroDoc};

/// Blocking work off the test's runtime.
async fn off<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

fn text_snapshot(text: &str) -> Vec<u8> {
    craft_cloud_agent::doc::plain_text_snapshot(text).unwrap()
}

/// An app's Loro document for an open live document.
struct AppDoc {
    loro: LoroDoc,
    doc: Document,
}

impl AppDoc {
    fn new(doc: Document, peer: u64) -> Self {
        let loro = LoroDoc::new();
        loro.set_peer_id(peer).unwrap();
        loro.import(doc.state()).unwrap();
        Self { loro, doc }
    }

    fn text(&self) -> String {
        self.loro.get_text("text").to_string()
    }

    /// Applies what came; the other events.
    fn pump(&self) -> Vec<Event> {
        let mut others = Vec::new();
        while let Some(event) = self.doc.try_recv() {
            match event {
                Event::Update(bytes) => {
                    self.loro.import(&bytes).unwrap();
                }
                other => others.push(other),
            }
        }
        others
    }

    /// Types at the end and sends it: its number.
    fn append(&self, s: &str) -> u64 {
        let before = self.loro.oplog_vv();
        let t = self.loro.get_text("text");
        t.insert(t.len_unicode(), s).unwrap();
        self.loro.commit();
        self.doc.send(&self.loro.export(ExportMode::updates(&before)).unwrap()).unwrap()
    }
}

/// Opens `path`, importing `content` if the file has never been live.
fn open(docs: &Documents, path: &str, content: &str) -> Document {
    match docs.open(path, "wordcraft").unwrap() {
        Opened::Live(doc) => doc,
        Opened::NeedsImport(importer) => importer.import(&text_snapshot(content)).unwrap(),
        Opened::NotLive(reason) => panic!("not live: {reason:?}"),
    }
}

/// Waits for `path` to be synced to `agent`'s folder.
async fn synced(agent: &Agent, path: &str) {
    let file = agent.folder.join(path);
    eventually(&format!("{path} reached {}", agent.folder.display()), move || file.exists().then_some(())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_opened_on_one_machine_is_live_on_the_other() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let (on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("notes.txt"), "from the file").unwrap();
    synced(&laptop, "notes.txt").await;
    // The pc opens it first: it imports it.
    let c = on_pc.clone();
    let (docs_pc, app_pc) = off(move || {
        let docs = c.documents().unwrap();
        let doc = open(&docs, "notes.txt", "from the file");
        (docs, AppDoc::new(doc, 11))
    })
    .await;
    assert_eq!(app_pc.text(), "from the file");
    // The laptop joins it.
    let c = on_laptop.clone();
    let (docs_laptop, app_laptop) = off(move || {
        let docs = c.documents().unwrap();
        let doc = open(&docs, "notes.txt", "never used");
        (docs, AppDoc::new(doc, 12))
    })
    .await;
    assert_eq!(app_laptop.text(), "from the file");
    // A keystroke on one shows up in the other's open document.
    let (app_pc, app_laptop) = (Arc::new(Mutex::new(app_pc)), Arc::new(Mutex::new(app_laptop)));
    let seq = app_pc.lock().unwrap().append(", typed on the pc");
    let a = app_pc.clone();
    eventually("the pc's change is acked", move || a.lock().unwrap().pump().contains(&Event::Acked(seq)).then_some(())).await;
    let l = app_laptop.clone();
    eventually("the laptop shows it", move || {
        let app = l.lock().unwrap();
        app.pump();
        (app.text() == "from the file, typed on the pc").then_some(())
    })
    .await;
    // Presence goes across too.
    app_laptop.lock().unwrap().doc.presence(b"cursor 4").unwrap();
    let a = app_pc.clone();
    eventually("the pc sees the laptop's cursor", move || a.lock().unwrap().pump().contains(&Event::Presence(b"cursor 4".to_vec())).then_some(()))
        .await;
    // Two apps on one machine share it at once (without the server's round trip).
    let c = on_laptop.clone();
    let second = off(move || {
        let docs = c.documents().unwrap();
        let doc = open(&docs, "notes.txt", "never used");
        (docs, AppDoc::new(doc, 13))
    })
    .await;
    assert_eq!(second.1.text(), "from the file, typed on the pc");
    app_laptop.lock().unwrap().append("!");
    let s = Arc::new(Mutex::new(second));
    let s2 = s.clone();
    eventually("the other app on the laptop gets it", move || {
        let app = &s2.lock().unwrap().1;
        app.pump();
        app.text().ends_with('!').then_some(())
    })
    .await;
    drop((docs_pc, docs_laptop, s));
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// A file not uploaded yet: the open starts a sync pass and waits for the file's node.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_not_uploaded_yet_is_synced_for_the_open() {
    let server = Server::new().await;
    let pc = Agent::signed_in(&server, "pc").await;
    let running = pc.run();
    let on_pc = pc.client().await;
    let c = on_pc.clone();
    eventually("online", move || c.status().ok()?.online.then_some(())).await;
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("fresh.txt"), "new").unwrap();
    let c = on_pc.clone();
    let opened = off(move || {
        let docs = c.documents().unwrap();
        let opened = docs.open("fresh.txt", "wordcraft").unwrap();
        matches!(opened, Opened::NeedsImport(_))
    })
    .await;
    assert!(opened, "found once synced, and asked for its import");
    // A path outside the folder, or of no file.
    let c = on_pc.clone();
    let (outside, missing) = off(move || {
        let docs = c.documents().unwrap();
        (docs.open("../outside.txt", "wordcraft").unwrap(), docs.open("nothing-here.txt", "wordcraft").unwrap())
    })
    .await;
    assert!(matches!(outside, Opened::NotLive(NotLiveReason::NotInFolder)), "{outside:?}");
    assert!(matches!(missing, Opened::NotLive(NotLiveReason::NotInFolder)), "{missing:?}");
    running.stop().await.0.unwrap();
}

/// Review: two machines open a never-live file at once and both import it: one import wins, both end on the same
/// document, and neither app is told its document is live before that is settled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_machines_importing_a_file_at_once_end_on_one_document() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let (on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("race.txt"), "x").unwrap();
    synced(&laptop, "race.txt").await;
    let (c1, c2) = (on_pc.clone(), on_laptop.clone());
    // Both opens first (both find no document), then both imports at once.
    let (o1, o2) = tokio::join!(
        off(move || {
            let docs = c1.documents().unwrap();
            let opened = docs.open("race.txt", "wordcraft").unwrap();
            (docs, opened)
        }),
        off(move || {
            let docs = c2.documents().unwrap();
            let opened = docs.open("race.txt", "wordcraft").unwrap();
            (docs, opened)
        })
    );
    let import = |(docs, opened): (Documents, Opened), content: &'static str| {
        off(move || {
            let doc = match opened {
                Opened::NeedsImport(importer) => importer.import(&text_snapshot(content)).unwrap(),
                Opened::Live(doc) => doc,
                Opened::NotLive(reason) => panic!("{reason:?}"),
            };
            (docs, AppDoc::new(doc, 21))
        })
    };
    let (a, b) = tokio::join!(import(o1, "the pc's import"), import(o2, "the laptop's import"));
    assert_eq!(a.1.doc.id(), b.1.doc.id(), "one document");
    assert_eq!(a.1.text(), b.1.text(), "the same import on both");
    assert!(a.1.text() == "the pc's import" || a.1.text() == "the laptop's import");
    drop((a, b));
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// Review: a file renamed, moved or deleted on another machine while it is open here. A rename or move keeps the
/// document (it belongs to the file's node); a delete leaves it working and says so; a new file at the same path is a
/// new document.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_renamed_moved_or_deleted_elsewhere_keeps_its_open_document() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let on_pc = pc.client().await;
    laptop.client().await;
    std::fs::create_dir_all(pc.folder.join("Old")).unwrap();
    std::fs::write(pc.folder.join("Old/a.txt"), "aaa").unwrap();
    synced(&laptop, "Old/a.txt").await;
    let c = on_pc.clone();
    let (docs, app) = off(move || {
        let docs = c.documents().unwrap();
        let doc = open(&docs, "Old/a.txt", "aaa");
        (docs, AppDoc::new(doc, 31))
    })
    .await;
    let first = app.doc.id();
    // Renamed and moved on the laptop.
    std::fs::create_dir_all(laptop.folder.join("New")).unwrap();
    std::fs::rename(laptop.folder.join("Old/a.txt"), laptop.folder.join("New/b.txt")).unwrap();
    synced(&pc, "New/b.txt").await;
    let docs = Arc::new(docs);
    let d = docs.clone();
    let again = off(move || {
        let doc = open(&d, "New/b.txt", "never used");
        doc.id()
    })
    .await;
    assert_eq!(again, first, "the same node, the same document");
    let app = Arc::new(Mutex::new(app));
    let seq = app.lock().unwrap().append(" still here");
    let a = app.clone();
    eventually("an edit after the move is acked", move || a.lock().unwrap().pump().contains(&Event::Acked(seq)).then_some(())).await;
    // Deleted on the laptop: the open document goes on, and says its file was deleted.
    std::fs::remove_file(laptop.folder.join("New/b.txt")).unwrap();
    let a = app.clone();
    eventually("the document says its file was deleted", move || {
        a.lock().unwrap().pump().iter().any(|e| matches!(e, Event::Status { deleted: true, .. })).then_some(())
    })
    .await;
    let seq = app.lock().unwrap().append(" after the delete");
    let a = app.clone();
    eventually("it still takes edits", move || a.lock().unwrap().pump().contains(&Event::Acked(seq)).then_some(())).await;
    // A new file at the old path is a new node, and a new document.
    std::fs::write(pc.folder.join("New/b.txt"), "new file").unwrap();
    let file = laptop.folder.join("New/b.txt");
    eventually("the new file reached the laptop", move || file.exists().then_some(())).await;
    let d = docs.clone();
    let fresh = eventually("the new file's document", move || match d.open("New/b.txt", "wordcraft").ok()? {
        Opened::NeedsImport(importer) => Some(importer.import(&text_snapshot("new file")).ok()?.id()),
        Opened::Live(doc) => Some(doc.id()),
        Opened::NotLive(_) => None,
    })
    .await;
    assert_ne!(fresh, first, "a new file, a new document");
    drop((docs, app));
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// The docs socket as a raw client sees it (the crate is well behaved).
mod raw {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    pub type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    pub async fn connect(agent: &super::Agent) -> Ws {
        let api: craft_cloud_proto::local::LocalApiFile =
            serde_json::from_slice(&std::fs::read(agent.data.path().join(craft_cloud_proto::local::LOCAL_API_FILE)).unwrap()).unwrap();
        let mut request = format!("ws://127.0.0.1:{}/v1/docs", api.port).into_client_request().unwrap();
        request.headers_mut().insert("authorization", format!("Bearer {}", api.token).parse().unwrap());
        tokio_tungstenite::connect_async(request).await.unwrap().0
    }

    pub async fn send_text(ws: &mut Ws, json: serde_json::Value) {
        ws.send(Message::Text(json.to_string().into())).await.unwrap();
    }

    pub async fn send_binary(ws: &mut Ws, bytes: Vec<u8>) {
        let _ = ws.send(Message::Binary(bytes.into())).await;
    }

    /// The next message, or `None` once the connection is closed (or nothing comes for 10 s).
    pub async fn next(ws: &mut Ws) -> Option<Message> {
        match tokio::time::timeout(std::time::Duration::from_secs(10), ws.next()).await {
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) | Err(_) => None,
            Ok(Some(Ok(message))) => Some(message),
        }
    }

    /// The next control message, skipping binary ones.
    pub async fn next_json(ws: &mut Ws) -> Option<serde_json::Value> {
        loop {
            match next(ws).await? {
                Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
                _ => continue,
            }
        }
    }
}

/// Review: an app that sends garbage binary frames is dropped; an update over 1 MiB is refused; one that never reads
/// is dropped; one that goes away mid-import leaves no half import on the server. The agent and the other apps go on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_app_that_misbehaves_is_refused_or_dropped_and_the_rest_go_on() {
    let server = Server::new().await;
    let pc = Agent::signed_in(&server, "pc").await;
    let running = pc.run();
    let on_pc = pc.client().await;
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("doc.txt"), "doc").unwrap();
    std::fs::write(pc.folder.join("other.txt"), "other").unwrap();
    let c = on_pc.clone();
    eventually("both files are synced", move || {
        let docs = c.documents().ok()?;
        matches!(docs.open("other.txt", "wordcraft").ok()?, Opened::NeedsImport(_)).then_some(())
    })
    .await;
    // A well-behaved app has a document open throughout.
    let c = on_pc.clone();
    let (good_docs, good) = off(move || {
        let docs = c.documents().unwrap();
        let doc = open(&docs, "doc.txt", "doc");
        (docs, AppDoc::new(doc, 41))
    })
    .await;
    // Garbage binary frames: a frame too short for its header, and one of a kind an app never sends.
    for junk in [vec![2, 0, 0], craft_cloud_proto::local::docs::encode_binary(1, 7, b"a state from the app")] {
        let mut ws = raw::connect(&pc).await;
        raw::send_binary(&mut ws, junk).await;
        assert!(raw::next(&mut ws).await.is_none(), "dropped");
    }
    // An update over 1 MiB: refused, and the connection goes on.
    let mut ws = raw::connect(&pc).await;
    raw::send_text(&mut ws, serde_json::json!({ "type": "open", "id": 1, "path": "doc.txt", "app": "wordcraft" })).await;
    let opened = raw::next_json(&mut ws).await.unwrap();
    assert_eq!(opened["type"], "opened", "{opened}");
    let huge = craft_cloud_proto::local::docs::encode_binary(2, 1, &vec![0u8; craft_cloud_proto::live::MAX_UPDATE_BYTES + 1]);
    raw::send_binary(&mut ws, huge).await;
    let refused = raw::next_json(&mut ws).await.unwrap();
    assert_eq!(
        (refused["type"].as_str(), refused["code"].as_str(), refused["seq"].as_u64()),
        (Some("error"), Some("too_large"), Some(1)),
        "{refused}"
    );
    // An update that isn't Loro: refused, the document as it was.
    raw::send_binary(&mut ws, craft_cloud_proto::local::docs::encode_binary(2, 1, b"not loro")).await;
    let refused = raw::next_json(&mut ws).await.unwrap();
    assert_eq!((refused["code"].as_str(), refused["seq"].as_u64()), (Some("invalid"), Some(2)), "{refused}");
    // A request that isn't one.
    futures_util::SinkExt::send(&mut ws, tokio_tungstenite::tungstenite::Message::Text("{\"type\":\"nonsense\"}".into())).await.unwrap();
    assert_eq!(raw::next_json(&mut ws).await.unwrap()["type"], "error");
    drop(ws);
    // Away mid-import: asked for the import, and gone without sending it.
    let mut ws = raw::connect(&pc).await;
    raw::send_text(&mut ws, serde_json::json!({ "type": "open", "id": 1, "path": "other.txt", "app": "wordcraft" })).await;
    assert_eq!(raw::next_json(&mut ws).await.unwrap()["type"], "needs_import");
    drop(ws);
    let doc = craft_cloud_proto::live::node_doc(
        server.meta.user_by_name("ana").await.unwrap().unwrap().space,
        *server_node(&server, "other.txt").await.as_ref().unwrap(),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let space = server.meta.user_by_name("ana").await.unwrap().unwrap().space;
    assert!(server.meta.live_doc(space, doc).await.unwrap().is_none(), "no half import on the server");
    // The next app to open it is asked again, and makes it live.
    let c = on_pc.clone();
    let other = off(move || {
        let docs = c.documents().unwrap();
        let doc = open(&docs, "other.txt", "other");
        (docs, AppDoc::new(doc, 42))
    })
    .await;
    assert_eq!(other.1.text(), "other");
    // An app that never reads while another floods its document with big changes: dropped (it can't hold the agent up).
    let mut lazy = raw::connect(&pc).await;
    raw::send_text(&mut lazy, serde_json::json!({ "type": "open", "id": 1, "path": "doc.txt", "app": "wordcraft" })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let good = Arc::new(Mutex::new(good));
    let g = good.clone();
    off(move || {
        let app = g.lock().unwrap();
        let chunk = "y".repeat(200_000);
        for _ in 0..60 {
            app.append(&chunk);
        }
    })
    .await;
    // The well-behaved app gets every ack meanwhile, and the agent stays up.
    let g = good.clone();
    eventually("the good app's changes are all acked", move || {
        let app = g.lock().unwrap();
        app.pump().contains(&Event::Acked(60)).then_some(())
    })
    .await;
    // Not reading for over 10 s (the agent's limit for one message) before looking.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let mut dropped = false;
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_secs(30), futures_util::StreamExt::next(&mut lazy)).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => {
                dropped = true;
                break;
            }
            Ok(Some(Ok(_))) => {}
            Err(_) => break,
        }
    }
    assert!(dropped, "the app that didn't read was dropped");
    let c = on_pc.clone();
    assert!(off(move || c.status()).await.unwrap().signed_in, "the agent goes on");
    drop((good_docs, good, other));
    running.stop().await.0.unwrap();
}

/// The node the server has at `path` (from the root).
async fn server_node(server: &Server, path: &str) -> Option<craft_cloud_proto::NodeId> {
    let space = server.meta.user_by_name("ana").await.unwrap()?.space;
    let tree = server.meta.tree(space, None, 5000).await.unwrap();
    let name = path.rsplit('/').next()?;
    tree.nodes.iter().find(|n| n.name.as_ref().is_some_and(|n| n.as_str() == name)).map(|n| n.id)
}

/// Runs the agent's command line with `args`, against `agent`'s data folder.
async fn cli(agent: &Agent, args: &[&str]) -> (Result<craft_cloud_agent::cli::Exit, craft_cloud_agent::cli::AgentError>, String) {
    use clap::Parser;
    let data = agent.data.path().display().to_string();
    let mut argv = vec!["craft-cloud-agent", "--data-dir", data.as_str()];
    argv.extend_from_slice(args);
    let parsed = craft_cloud_agent::cli::Cli::try_parse_from(argv).unwrap();
    let mut out = Vec::new();
    let exit = craft_cloud_agent::cli::run(parsed, &mut "".as_bytes(), &mut out).await;
    (exit, String::from_utf8(out).unwrap())
}

/// The `doc` tool: imports a plain-text file, prints it, appends to it (on every machine), and refuses a file that
/// isn't UTF-8. It never writes the file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_doc_command_reads_and_appends_like_an_app() {
    use craft_cloud_agent::cli::{AgentError, Exit};
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    pc.client().await;
    laptop.client().await;
    std::fs::create_dir_all(&pc.folder).unwrap();
    std::fs::write(pc.folder.join("todo.txt"), "milk\n").unwrap();
    std::fs::write(pc.folder.join("binary.bin"), [0xff, 0xfe, 0x00]).unwrap();
    synced(&laptop, "todo.txt").await;
    synced(&laptop, "binary.bin").await;
    let (exit, out) = cli(&pc, &["doc", "todo.txt"]).await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert_eq!(out, "milk\n\n");
    let absolute = laptop.folder.join("todo.txt").display().to_string();
    let (exit, out) = cli(&laptop, &["doc", &absolute, "--append", "eggs\n"]).await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert_eq!(out, "milk\neggs\n\n");
    let p = Arc::new(pc);
    let pp = p.clone();
    // (The change reaches the pc as it comes; `doc` there sees it once it did.)
    let text = eventually("the pc has the laptop's change", move || {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        let (exit, out) = runtime.block_on(cli(&pp, &["doc", "todo.txt"]));
        (exit.ok()? == Exit::Done && out.contains("eggs")).then_some(out)
    })
    .await;
    assert_eq!(text, "milk\neggs\n\n");
    assert_eq!(std::fs::read_to_string(p.folder.join("todo.txt")).unwrap(), "milk\n", "the file is never written");
    let (exit, _) = cli(&p, &["doc", "binary.bin"]).await;
    assert!(matches!(&exit, Err(AgentError::Usage(m)) if m.contains("UTF-8")), "{exit:?}");
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// Signed out, every open is answered `not_live {not_signed_in}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signed_out_every_file_is_a_plain_file() {
    let agent = Agent::signed_out();
    let running = agent.run();
    let client = agent.client().await;
    let opened = off(move || client.documents().unwrap().open("a.txt", "wordcraft").unwrap()).await;
    assert!(matches!(opened, Opened::NotLive(NotLiveReason::NotSignedIn)), "{opened:?}");
    running.stop().await.0.unwrap();
}
