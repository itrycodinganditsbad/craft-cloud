//! Recents end to end: running agents of one account against a real server, driven through `craft-cloud-app` as an
//! app would. An entry added on one machine reaches the other's open list; one added offline (the server cut off)
//! reaches it once the server is back; a machine that starts later catches up; signing out ends the watch.

mod looping;

use std::time::Duration;

use craft_cloud_app::{Client, Error, LoginRequest, NewRecent, RecentEntry, RecentsEvent, RecentsWatch, codes};
use looping::{Agent, PASSWORD, Server, eventually};

fn new(path: &str) -> NewRecent {
    NewRecent { path: path.into(), title: None, saved: false }
}

fn paths(entries: &[RecentEntry]) -> Vec<String> {
    entries.iter().map(|e| e.path.clone()).collect()
}

/// Blocking work off the test's runtime.
async fn off<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entry_added_on_one_machine_shows_up_in_the_others_open_list() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let (on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    let status = {
        let c = on_pc.clone();
        off(move || c.status()).await.unwrap()
    };
    assert!(status.signed_in && status.machine.as_deref() == Some("pc"));
    let c = on_laptop.clone();
    let watch = off(move || c.watch_recents("wordcraft")).await.unwrap();
    assert!(watch.entries().is_empty());

    let c = on_pc.clone();
    let added = off(move || c.add_recent("wordcraft", NewRecent { path: "Letters/doc1.docx".into(), title: Some("Doc 1".into()), saved: true }))
        .await
        .unwrap();
    assert_eq!((added.path.as_str(), added.title.as_str(), added.machine.as_str()), ("Letters/doc1.docx", "Doc 1", "pc"));
    assert!(added.saved_ms.is_some());
    // The laptop's open list gets it, as a change event.
    let watch = std::sync::Arc::new(std::sync::Mutex::new(watch));
    let w = watch.clone();
    let list = eventually("the laptop's list has doc1", move || match w.lock().unwrap().events().recv_timeout(Duration::from_millis(100)) {
        Ok(RecentsEvent::Changed(entries)) if !entries.is_empty() => Some(entries),
        _ => None,
    })
    .await;
    assert_eq!(list, vec![added.clone()]);
    // Another app's list is its own.
    let c = on_laptop.clone();
    assert!(off(move || c.recents("gridcraft")).await.unwrap().is_empty());
    // The same file again (opened on the laptop): one entry, the newest.
    let c = on_laptop.clone();
    let again = off(move || c.add_recent("wordcraft", new("Letters/doc1.docx"))).await.unwrap();
    assert_eq!(again.saved_ms, added.saved_ms, "opened only: the save time is kept");
    let c = on_pc.clone();
    let mine = eventually("the pc sees the laptop's open", move || {
        let list = c.recents("wordcraft").ok()?;
        (list.first()?.machine == "laptop").then_some(list)
    })
    .await;
    assert_eq!(paths(&mine), vec!["Letters/doc1.docx"]);
    // Bad paths are refused.
    let c = on_pc.clone();
    let refused = off(move || c.add_recent("wordcraft", new("../outside.txt"))).await;
    assert!(matches!(refused, Err(Error::Refused { ref code, .. }) if code == "invalid"), "{refused:?}");
    drop(watch);
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entry_added_offline_reaches_the_others_once_the_server_is_back() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let (on_pc, on_laptop) = (pc.client().await, laptop.client().await);
    let c = on_pc.clone();
    eventually("the pc is online", move || c.status().ok()?.online.then_some(())).await;
    server.gate.down();
    let c = on_pc.clone();
    eventually("the pc sees it is offline", move || (!c.status().ok()?.online).then_some(())).await;
    // Offline: the add is kept, and the pc's own list has it at once.
    let c = on_pc.clone();
    off(move || c.add_recent("wordcraft", new("offline.docx"))).await.unwrap();
    let c = on_pc.clone();
    assert_eq!(paths(&off(move || c.recents("wordcraft")).await.unwrap()), vec!["offline.docx"]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    let c = on_laptop.clone();
    assert!(off(move || c.recents("wordcraft")).await.unwrap().is_empty(), "not there while the server is cut off");
    // A restart while offline keeps it (it is on disk).
    run_pc.stop().await.0.unwrap();
    let run_pc = pc.run();
    let on_pc = pc.client().await;
    let c = on_pc.clone();
    assert_eq!(paths(&off(move || c.recents("wordcraft")).await.unwrap()), vec!["offline.docx"]);
    server.gate.up();
    let c = on_laptop.clone();
    let list = eventually("the laptop gets it once the server is back", move || {
        let list = c.recents("wordcraft").ok()?;
        (!list.is_empty()).then_some(list)
    })
    .await;
    assert_eq!(paths(&list), vec!["offline.docx"]);
    // A third machine, started now, catches up.
    let desk = Agent::signed_in(&server, "desk").await;
    let run_desk = desk.run();
    let on_desk = desk.client().await;
    let c = on_desk.clone();
    let list = eventually("a new machine catches up", move || {
        let list = c.recents("wordcraft").ok()?;
        (!list.is_empty()).then_some(list)
    })
    .await;
    assert_eq!(paths(&list), vec!["offline.docx"]);
    for running in [run_pc, run_laptop, run_desk] {
        running.stop().await.0.unwrap();
    }
}

/// A local client that sends many requests without reading the answers must not deadlock its connection
/// (as an agent that waits to queue an answer on a queue only it drains would). It must get every answer, or be
/// dropped as too slow; never hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_doesnt_read_never_hangs_the_recents_socket() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let server = Server::new().await;
    let pc = Agent::signed_in(&server, "pc").await;
    let running = pc.run();
    pc.client().await;
    let api: craft_cloud_proto::local::LocalApiFile =
        serde_json::from_slice(&std::fs::read(pc.data.path().join(craft_cloud_proto::local::LOCAL_API_FILE)).unwrap()).unwrap();
    let mut request = format!("ws://127.0.0.1:{}/v1/recents", api.port).into_client_request().unwrap();
    request.headers_mut().insert("authorization", format!("Bearer {}", api.token).parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    // Many follows of one list, then adds to it: every add is a new list for every follow, all queued at once.
    for _ in 0..200 {
        ws.feed(Message::Text(r#"{"type":"subscribe","app":"wordcraft"}"#.into())).await.unwrap();
    }
    for id in 0..50 {
        ws.feed(Message::Text(format!(r#"{{"type":"add","id":{id},"app":"wordcraft","entry":{{"path":"f{id}.txt"}}}}"#).into())).await.unwrap();
    }
    ws.flush().await.unwrap();
    let mut added = 0;
    let read = tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(Ok(message)) = ws.next().await {
            if let Message::Text(text) = message
                && text.contains(r#""type":"added""#)
            {
                added += 1;
                if added == 50 {
                    return;
                }
            }
        }
    })
    .await;
    assert!(read.is_ok(), "the socket hung: {added} of 50 adds answered in 20 s");
    running.stop().await.0.unwrap();
}

/// Output shared with a command that runs on (`--watch`).
#[derive(Clone, Default)]
struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Shared {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A watch outlives the agent. It says when the agent goes (stopped, or signed out) and connects again by
/// itself once one runs (on a new port, with a new token), with the list as it is then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watch_reconnects_after_the_agent_restarts() {
    let server = Server::new().await;
    let (pc, laptop) = (Agent::signed_in(&server, "pc").await, Agent::signed_in(&server, "laptop").await);
    let (run_pc, run_laptop) = (pc.run(), laptop.run());
    let on_pc = pc.client().await;
    let c = on_pc.clone();
    let watch = std::sync::Arc::new(std::sync::Mutex::new(off(move || c.watch_recents("wordcraft")).await.unwrap()));
    let next = |watch: &std::sync::Arc<std::sync::Mutex<RecentsWatch>>, what: &'static str, want: fn(&RecentsEvent) -> bool| {
        let w = watch.clone();
        eventually(what, move || match w.lock().unwrap().events().recv_timeout(Duration::from_millis(100)) {
            Ok(e) if want(&e) => Some(()),
            _ => None,
        })
    };
    run_pc.stop().await.0.unwrap();
    next(&watch, "the watch says the agent is away", |e| matches!(e, RecentsEvent::Unavailable(_))).await;
    // An add on the other machine meanwhile; the agent starts again on another port.
    let on_laptop = laptop.client().await;
    let c = on_laptop.clone();
    off(move || c.add_recent("wordcraft", new("while-away.txt"))).await.unwrap();
    let run_pc = pc.run();
    next(&watch, "the watch is back", |e| matches!(e, RecentsEvent::Available)).await;
    let w = watch.clone();
    eventually("the list has what came meanwhile", move || w.lock().unwrap().entries().iter().any(|e| e.path == "while-away.txt").then_some(()))
        .await;
    let w = watch.clone();
    let added = off(move || w.lock().unwrap().add(new("after.txt"))).await.unwrap();
    assert_eq!(added.path, "after.txt", "adds work on the new connection");
    run_pc.stop().await.0.unwrap();
    run_laptop.stop().await.0.unwrap();
}

/// The CLI's commands, as a user types them.
async fn cli(agent: &Agent, args: &[&str], stdin: &str) -> (Result<craft_cloud_agent::cli::Exit, craft_cloud_agent::cli::AgentError>, String) {
    use clap::Parser;
    let data = agent.data.path().display().to_string();
    let mut argv = vec!["craft-cloud-agent", "--data-dir", data.as_str()];
    argv.extend_from_slice(args);
    let parsed = craft_cloud_agent::cli::Cli::try_parse_from(argv).unwrap();
    let mut out = Vec::new();
    let exit = craft_cloud_agent::cli::run(parsed, &mut stdin.as_bytes(), &mut out).await;
    (exit, String::from_utf8(out).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cli_signs_in_through_a_running_agent_and_shows_and_adds_recents() {
    use craft_cloud_agent::cli::{AgentError, Exit};
    let server = Server::new().await;
    let agent = Agent::signed_out();
    // Not running: `recents` says how to start it.
    let (exit, _) = cli(&agent, &["recents", "wordcraft"], "").await;
    assert!(matches!(&exit, Err(AgentError::Usage(m)) if m.contains("craft-cloud-agent run")), "{exit:?}");
    let running = agent.run();
    agent.client().await;
    // `login` while `run` runs (it holds the data folder): the running agent signs in.
    let folder = agent.folder.display().to_string();
    let (exit, out) =
        cli(&agent, &["login", &server.url, "--user", "ana", "--folder", &folder, "--device-name", "pc"], &format!("{PASSWORD}\n")).await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("through the running agent"), "{out}");
    let (exit, out) = cli(&agent, &["recents", "wordcraft"], "").await;
    assert_eq!(exit.unwrap(), Exit::Done);
    assert!(out.contains("no recent files for wordcraft"), "{out}");
    // A path relative to the folder, or absolute inside it.
    let (exit, out) = cli(&agent, &["recents", "wordcraft", "add", "Letters/doc1.docx", "--saved"], "").await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    let inside = agent.folder.join("Notes").join("todo.txt").display().to_string();
    let (exit, out) = cli(&agent, &["recents", "wordcraft", "add", &inside], "").await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    let (exit, out) = cli(&agent, &["recents", "wordcraft", "add", "/elsewhere/x.txt"], "").await;
    assert!(matches!(&exit, Err(AgentError::Usage(m)) if m.contains("isn't inside the synced folder")), "{exit:?} {out}");
    let (_, out) = cli(&agent, &["recents", "wordcraft"], "").await;
    let todo = out.find("Notes/todo.txt").unwrap();
    let doc1 = out.find("Letters/doc1.docx").unwrap();
    assert!(todo < doc1, "newest first: {out}");
    assert!(out.contains("(on pc, saved"), "{out}");
    // `--watch` shows the list again after every change; through a sign-out and a sign-in it says so and goes on.
    let client = agent.client().await;
    let shown = Shared::default();
    let watching = {
        use clap::Parser;
        let data = agent.data.path().display().to_string();
        let parsed =
            craft_cloud_agent::cli::Cli::try_parse_from(["craft-cloud-agent", "--data-dir", data.as_str(), "recents", "wordcraft", "--watch"])
                .unwrap();
        let mut out = shown.clone();
        tokio::spawn(async move { craft_cloud_agent::cli::run(parsed, &mut "".as_bytes(), &mut out).await })
    };
    let s = shown.clone();
    eventually("the watch shows the list", move || s.text().contains("Letters/doc1.docx").then_some(())).await;
    let c = client.clone();
    off(move || c.add_recent("wordcraft", new("watched.txt"))).await.unwrap();
    let s = shown.clone();
    eventually("the watch shows the change", move || s.text().split("--").nth(1).is_some_and(|t| t.contains("watched.txt")).then_some(())).await;
    let c = client.clone();
    off(move || c.logout(false)).await.unwrap();
    let s = shown.clone();
    eventually("the watch says Craft Cloud is away", move || s.text().contains("isn't available").then_some(())).await;
    // Signed in again (through the running agent): the watch is back by itself.
    let (exit, out) = cli(&agent, &["login", &server.url, "--user", "ana", "--folder", &folder, "--adopt"], &format!("{PASSWORD}\n")).await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    let s = shown.clone();
    eventually("the watch is back", move || s.text().contains("Craft Cloud is back").then_some(())).await;
    watching.abort();
    // Then `logout` while `run` runs: through it too.
    let (exit, out) = cli(&agent, &["logout"], "").await;
    assert_eq!(exit.unwrap(), Exit::Done, "{out}");
    assert!(out.contains("signed out (through the running agent)"), "{out}");
    assert!(agent.data.load_account().unwrap().is_none());
    running.stop().await.0.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_crate_finds_no_agent_and_signs_in_and_out_the_one_it_finds() {
    let empty = tempfile::tempdir().unwrap();
    let dir = empty.path().to_owned();
    assert!(matches!(off(move || Client::connect_in(&dir)).await, Err(Error::NotRunning(_))));
    // A file a crashed agent left behind: nobody answers on its port.
    let stale = serde_json::json!({ "schema": 1, "port": 1, "token": "x", "pid": 1 });
    std::fs::write(empty.path().join("local-api.json"), stale.to_string()).unwrap();
    let dir = empty.path().to_owned();
    assert!(matches!(off(move || Client::connect_in(&dir)).await, Err(Error::NotRunning(_))));

    let server = Server::new().await;
    let agent = Agent::signed_out();
    let running = agent.run();
    let client = agent.client().await;
    let c = client.clone();
    assert!(!off(move || c.status()).await.unwrap().signed_in);
    let c = client.clone();
    let signed_out = off(move || c.watch_recents("wordcraft")).await;
    assert!(matches!(signed_out, Err(Error::Refused { ref code, .. }) if code == codes::NOT_SIGNED_IN), "{signed_out:?}");
    let login = |password: &str| LoginRequest {
        server: server.url.clone(),
        user: "ana".into(),
        password: password.into(),
        folder: Some(agent.folder.display().to_string()),
        device_name: Some("pc".into()),
        adopt: false,
    };
    let (c, wrong) = (client.clone(), login("wrong password"));
    assert!(matches!(off(move || c.login(&wrong)).await, Err(Error::Refused { ref code, .. }) if code == codes::WRONG_PASSWORD));
    let (c, right) = (client.clone(), login(PASSWORD));
    assert!(off(move || c.login(&right)).await.unwrap().signed_in);
    let c = client.clone();
    let watch = off(move || c.watch_recents("wordcraft")).await.unwrap();
    let c = client.clone();
    assert!(!off(move || c.logout(false)).await.unwrap().signed_in);
    // The watch hears that recents are unavailable.
    let away = off(move || {
        loop {
            match watch.events().recv_timeout(Duration::from_secs(10)) {
                Ok(RecentsEvent::Unavailable(why)) => return Some((why, watch)),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    })
    .await;
    let (_, watch) = away.expect("the watch says recents are away with the sign-out");
    // An add while away is refused at once.
    let refused = off(move || watch.add(new("x.txt"))).await;
    assert!(matches!(refused, Err(Error::NotRunning(_))), "{refused:?}");
    running.stop().await.0.unwrap();
}
