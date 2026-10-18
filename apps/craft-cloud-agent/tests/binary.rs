//! The built binary, run as a user would: exit codes of usage errors and help, and `status` with nothing set up.

use std::process::{Command, Output};

fn agent(args: &[&str], data: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_craft-cloud-agent")).args(args).env("CRAFT_CLOUD_AGENT_DIR", data).output().unwrap()
}

#[test]
fn a_typo_is_a_failure_not_a_wait_for_approval() {
    let dir = tempfile::tempdir().unwrap();
    let out = agent(&["sync", "--aprove-deletes"], dir.path());
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("--aprove-deletes"));
    assert_eq!(agent(&[], dir.path()).status.code(), Some(1), "no command at all");
}

#[test]
fn help_and_version_exit_zero() {
    let dir = tempfile::tempdir().unwrap();
    let help = agent(&["--help"], dir.path());
    assert_eq!(help.status.code(), Some(0));
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("Exit codes") && text.contains("3 "), "{text}");
    assert_eq!(agent(&["--version"], dir.path()).status.code(), Some(0));
    assert_eq!(agent(&["sync", "--help"], dir.path()).status.code(), Some(0));
}

#[test]
fn status_with_an_empty_data_dir_says_not_signed_in() {
    let dir = tempfile::tempdir().unwrap();
    let out = agent(&["status"], &dir.path().join("data"));
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("not signed in"));
}

/// The next line a child printed, within 30 s.
async fn next_line(lines: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(30), lines.recv()).await.unwrap().unwrap()
}

/// Kills a child process when the test ends, even if it panics.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `add-computer` on a signed-in machine whose `run` runs prints a join string; `join` with it in a second data folder
/// signs that one in, and `add-computer` says it joined.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_computer_then_join_on_a_second_machine() {
    use std::io::{BufRead, Write};
    use std::process::Stdio;
    let password = "correct horse battery";
    let (db, blobs) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let meta = craft_cloud_meta::Meta::open(&format!("sqlite:{}", db.path().join("meta.db").display())).await.unwrap();
    meta.create_user("ana", &craft_cloud_server::auth::hash_password(password).unwrap(), craft_cloud_meta::Role::Member, 0).await.unwrap();
    let store = craft_cloud_store::Store::new(craft_cloud_store::disk::DiskBackend::new(blobs.path()).unwrap());
    let state = craft_cloud_server::AppState::new(meta.clone(), store, vec![]).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(craft_cloud_server::serve(state, listener, std::future::pending()));

    let dir = tempfile::tempdir().unwrap();
    let (data_a, data_b) = (dir.path().join("a"), dir.path().join("b"));
    let exe = env!("CARGO_BIN_EXE_craft-cloud-agent");
    let run = |args: Vec<String>, data: std::path::PathBuf, input: &'static str| async move {
        tokio::task::spawn_blocking(move || {
            let mut child = Command::new(exe)
                .args(&args)
                .env("CRAFT_CLOUD_AGENT_DIR", &data)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    };
    let folder_a = dir.path().join("Folder A").display().to_string();
    let login =
        run(vec!["login".into(), url.clone(), "--user".into(), "ana".into(), "--folder".into(), folder_a], data_a.clone(), "correct horse battery\n")
            .await;
    assert!(login.status.success(), "{login:?}");
    let _running =
        Child(Command::new(exe).arg("run").env("CRAFT_CLOUD_AGENT_DIR", &data_a).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let api = data_a.join(craft_cloud_proto::local::LOCAL_API_FILE);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !api.exists() {
        assert!(std::time::Instant::now() < deadline, "A's local API comes up");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let mut adding = Child(
        Command::new(exe).arg("add-computer").env("CRAFT_CLOUD_AGENT_DIR", &data_a).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap(),
    );
    let stdout = adding.0.stdout.take().unwrap();
    let (lines_tx, mut lines) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if lines_tx.send(line).is_err() {
                return;
            }
        }
    });
    let first = next_line(&mut lines).await;
    let join = first.split_once("enter: ").map(|(_, j)| j.trim().to_owned()).unwrap_or_else(|| panic!("{first}"));
    assert!(join.starts_with("http://127.0.0.1:"), "{join}");

    let folder_b = dir.path().join("Folder B").display().to_string();
    let joined = run(vec!["join".into(), join, "--folder".into(), folder_b, "--name".into(), "lptp1".into()], data_b.clone(), "").await;
    assert!(joined.status.success(), "{joined:?}");
    assert!(String::from_utf8_lossy(&joined.stdout).contains("joined as ana"), "{joined:?}");
    let mut said = Vec::new();
    loop {
        let line = next_line(&mut lines).await;
        if line.contains("joined") {
            assert_eq!(line, "lptp1 joined");
            break;
        }
        said.push(line);
        assert!(said.len() < 10, "{said:?}");
    }
    let exit = tokio::task::spawn_blocking(move || adding.0.wait().unwrap()).await.unwrap();
    assert!(exit.success(), "{exit:?}");
    let status = run(vec!["status".into()], data_b, "").await;
    assert!(String::from_utf8_lossy(&status.stdout).contains("signed in as ana"), "{status:?}");
}

/// The background build runs the same commands, and writes what they say to `run.log` in the data folder.
#[test]
fn the_background_build_writes_to_its_log() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = Command::new(env!("CARGO_BIN_EXE_craft-cloud-agent-bg")).arg("status").env("CRAFT_CLOUD_AGENT_DIR", &data).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert!(out.stdout.is_empty(), "nothing on the console");
    let log = std::fs::read_to_string(data.join("run.log")).unwrap();
    assert!(log.contains("not signed in"), "{log}");
    let failed = Command::new(env!("CARGO_BIN_EXE_craft-cloud-agent-bg")).arg("sync").env("CRAFT_CLOUD_AGENT_DIR", &data).output().unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(std::fs::read_to_string(data.join("run.log")).unwrap().contains("error: not signed in"), "errors go there too");
}

/// A stop that needs the user exits 3 from the real binary, with the reason on standard error.
#[test]
fn a_damaged_account_file_exits_3() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("account.json"), "{not json").unwrap();
    for command in ["sync", "status"] {
        let out = agent(&[command], dir.path());
        assert_eq!(out.status.code(), Some(3), "{command}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("account.json") && err.contains("move it away"), "{command}: {err}");
    }
}
