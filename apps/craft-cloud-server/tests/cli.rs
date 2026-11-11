//! The admin command line, called in-process, and the real binary started with `init` + `serve`.

use std::io::Cursor;
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
use craft_cloud_proto::join::{Code, INVITE_CHARS, JoinString};
use craft_cloud_server::cli::{Cli, CliError, run};
use craft_cloud_server::config::{Config, ConfigError};

async fn cli(args: &[&str], stdin: &str) -> Result<String, CliError> {
    let parsed = Cli::try_parse_from(std::iter::once("craft-cloud-server").chain(args.iter().copied())).unwrap();
    let mut out = Vec::new();
    run(parsed.command, &mut Cursor::new(stdin.as_bytes().to_vec()), &mut out).await?;
    Ok(String::from_utf8(out).unwrap())
}

#[tokio::test]
async fn init_then_manage_users_devices_and_epochs() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let dir_s = dir.to_str().unwrap();
    let out = cli(&["init", "--dir", dir_s, "--admin", "Ana"], "admin password 1\n").await.unwrap();
    assert!(out.contains("serve --config"), "{out}");
    let config_path = dir.join("config.toml");
    let config = Config::load(&config_path).unwrap();
    assert!(config.listen.ip().is_loopback());
    let c = config_path.to_str().unwrap();

    // init refuses a directory that isn't empty.
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "x"], "admin password 1\n").await, Err(CliError::Usage(_))));

    assert!(cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap().contains("added bob (member)"));
    assert!(matches!(cli(&["user", "--config", c, "add", "BOB"], "another password\n").await, Err(CliError::Meta(_))));
    assert!(matches!(cli(&["user", "--config", c, "add", "carol"], "short\n").await, Err(CliError::Usage(_))));
    assert!(cli(&["user", "--config", c, "reset-password", "ana"], "a brand new password\n").await.unwrap().contains("password changed for ana"));
    assert_eq!(cli(&["device", "--config", c, "list", "bob"], "").await.unwrap(), "");
    assert!(cli(&["reset-epoch", "--config", c, "bob"], "").await.unwrap().starts_with("new epoch "));
    assert!(matches!(cli(&["reset-epoch", "--config", c, "nobody"], "").await, Err(CliError::Usage(_))));

    // --config may also follow the action.
    assert!(cli(&["user", "add", "--config", c, "dora"], "dora password 1\n").await.unwrap().contains("added dora (member)"));
    assert_eq!(cli(&["device", "list", "--config", c, "dora"], "").await.unwrap(), "");
    assert!(matches!(cli(&["user", "add", "erin"], "erin password 1\n").await, Err(CliError::Usage(m)) if m.contains("--config")));
}

#[tokio::test]
async fn reset_epoch_all_resets_every_space() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap();

    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let spaces = meta.all_spaces().await.unwrap();
    assert_eq!(spaces.len(), 2);
    let mut before = Vec::new();
    for space in &spaces {
        before.push(meta.space_info(*space).await.unwrap().epoch);
    }
    let out = cli(&["reset-epoch", "--config", c, "--all"], "").await.unwrap();
    assert!(out.contains("reset 2 space(s)"), "{out}");
    for (space, old) in spaces.iter().zip(before) {
        assert_ne!(meta.space_info(*space).await.unwrap().epoch, old);
    }
}

/// `live reset-doc` empties a full recents document, which takes appends again.
#[tokio::test]
async fn live_reset_doc_empties_a_full_document() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let meta = meta.with_live_doc_cap(10);
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let device = meta.create_device(ana.id, "pc", &[3; 32], 0).await.unwrap();
    let app = craft_cloud_proto::live::AppName::new("wordcraft").unwrap();
    let doc = craft_cloud_proto::live::recents_doc(ana.space, &app);
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, 0).await.unwrap();
    meta.append_live_update(ana.space, doc, device, 1, &[1; 20], 0).await.unwrap();
    assert!(meta.append_live_update(ana.space, doc, device, 2, &[2], 0).await.is_err(), "full");
    let out = cli(&["live", "--config", c, "reset-doc", "ana", "wordcraft"], "").await.unwrap();
    assert!(out.contains("emptied the wordcraft recents of ana"), "{out}");
    assert_eq!(meta.live_doc(ana.space, doc).await.unwrap().unwrap().head, 0);
    assert_eq!(meta.append_live_update(ana.space, doc, device, 2, &[2], 0).await.unwrap(), 1, "open again");
    assert!(cli(&["live", "--config", c, "reset-doc", "ana", "Bad App"], "").await.is_err());
    assert!(matches!(cli(&["live", "--config", c, "reset-doc", "nobody", "wordcraft"], "").await, Err(CliError::Usage(_))));
}

/// `live reset-doc` takes a document's ID too (a document of a file has no app name to find it by), and drops its
/// snapshot with its log.
#[tokio::test]
async fn live_reset_doc_takes_a_document_id_and_drops_its_snapshot() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let device = meta.create_device(ana.id, "pc", &[3; 32], 0).await.unwrap();
    let doc = craft_cloud_proto::DocId::generate();
    meta.create_live_doc(ana.space, doc, "recents:wordcraft", None, 0).await.unwrap();
    meta.append_live_update(ana.space, doc, device, 1, &[1; 20], 0).await.unwrap();
    // A snapshot as an agent uploads one: a chunk, and the manifest naming it.
    let chunk = craft_cloud_proto::ChunkId::from_bytes([7; 32]);
    meta.add_chunk(ana.space, &chunk, 31, 0).await.unwrap();
    let manifest = craft_cloud_proto::Manifest::new(
        craft_cloud_proto::ChunkerParamsId::new("fastcdc-v1").unwrap(),
        30,
        craft_cloud_proto::FileHash::from_bytes([7; 32]),
        vec![craft_cloud_proto::ChunkRef { id: chunk, len: 30 }],
    )
    .unwrap();
    meta.add_manifest(ana.space, &manifest, 100, 0).await.unwrap();
    assert!(meta.compact_live_doc(ana.space, doc, 1, &manifest.id()).await.unwrap());
    let out = cli(&["live", "--config", c, "reset-doc", "ana", &doc.to_string()], "").await.unwrap();
    assert!(out.contains(&format!("emptied the live document {doc} of ana")), "{out}");
    let reset = meta.live_doc(ana.space, doc).await.unwrap().unwrap();
    assert_eq!((reset.head, reset.base, reset.has_snapshot), (0, 0, false));
    let missing = craft_cloud_proto::DocId::generate().to_string();
    assert!(cli(&["live", "--config", c, "reset-doc", "ana", &missing], "").await.is_err(), "no such document");
}

/// `user reset-password` signs out every device of the account (and deletes its open invite and pairings) and says
/// how many; `--keep-devices` leaves the devices; `device revoke-all` signs out every device; an unknown user is an
/// error, not a panic.
#[tokio::test]
async fn reset_password_signs_out_every_device_unless_kept() {
    use craft_cloud_meta::RevokeReason;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(
        &["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"],
        "admin password 1
",
    )
    .await
    .unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    for name in ["bob", "carol", "dora"] {
        cli(
            &["user", "--config", c, "add", name],
            "some password 1
",
        )
        .await
        .unwrap();
    }
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let devices = |name: &'static str, n: u8| {
        let meta = meta.clone();
        async move {
            let user = meta.user_by_name(name).await.unwrap().unwrap();
            let mut ids = Vec::new();
            for _ in 0..n {
                let token = *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes();
                ids.push(meta.create_device(user.id, "pc", &token, 0).await.unwrap());
            }
            (user, ids)
        }
    };

    // The default: every device is signed out, the invite and the pairing are gone, the count is printed.
    let (bob, bobs) = devices("bob", 3).await;
    let invite = Code::generate(INVITE_CHARS).unwrap();
    meta.create_invite("bob", &invite.hash(), 0, 1_000_000).await.unwrap();
    let pairing = Code::generate(craft_cloud_proto::join::PAIRING_CHARS).unwrap();
    let pairing_id = meta.create_pairing(bob.id, bobs[0], &pairing.hash(), 0).await.unwrap();
    let out = cli(
        &["user", "--config", c, "reset-password", "bob"],
        "a brand new password
",
    )
    .await
    .unwrap();
    assert!(out.contains("password changed for bob") && out.contains("signed out 3 device(s)"), "{out}");
    for id in &bobs {
        assert!(!meta.device_active(*id).await.unwrap());
    }
    let listed = meta.list_devices(bob.id).await.unwrap();
    assert!(listed.iter().all(|d| d.revoked && d.revoke_reason == Some(RevokeReason::Reset) && d.revoked_ms.is_some()), "{listed:?}");
    assert_eq!(meta.invite_needs_password(&invite.hash(), 1).await.unwrap(), None);
    assert_eq!(meta.pairing_status(bob.id, pairing_id, 1).await.unwrap(), None);
    assert!(meta.account(bob.id).await.unwrap().password_changed_ms.is_some());
    let shown = cli(&["device", "--config", c, "list", "bob"], "").await.unwrap();
    assert_eq!(shown.matches("(revoked)").count(), 3, "{shown}");

    // `--keep-devices`, before or after the name: the devices stay, the password still changes.
    let (carol, carols) = devices("carol", 2).await;
    let out = cli(
        &["user", "--config", c, "reset-password", "carol", "--keep-devices"],
        "a brand new password
",
    )
    .await
    .unwrap();
    assert!(out.contains("password changed for carol") && out.contains("stay signed in"), "{out}");
    for id in &carols {
        assert!(meta.device_active(*id).await.unwrap());
    }
    assert!(meta.account(carol.id).await.unwrap().password_changed_ms.is_some());
    let out = cli(
        &["user", "--config", c, "reset-password", "--keep-devices", "carol"],
        "another new password
",
    )
    .await
    .unwrap();
    assert!(out.contains("stay signed in"), "{out}");

    // `device revoke-all`.
    let (dora, doras) = devices("dora", 2).await;
    let out = cli(&["device", "--config", c, "revoke-all", "dora"], "").await.unwrap();
    assert!(out.contains("revoked 2 device(s) of dora"), "{out}");
    for id in &doras {
        assert!(!meta.device_active(*id).await.unwrap());
    }
    assert!(meta.list_devices(dora.id).await.unwrap().iter().all(|d| d.revoke_reason == Some(RevokeReason::Admin)));
    assert!(cli(&["device", "--config", c, "revoke-all", "dora"], "").await.unwrap().contains("revoked 0 device(s) of dora"));
    // Another account's devices are untouched.
    assert!(meta.device_active(carols[0]).await.unwrap());

    // An unknown user is an error, and nothing is read or changed.
    assert!(matches!(cli(&["user", "--config", c, "reset-password", "nobody"], "a brand new password
").await, Err(CliError::Usage(m)) if m.contains("nobody")));
    assert!(matches!(cli(&["device", "--config", c, "revoke-all", "nobody"], "").await, Err(CliError::Usage(m)) if m.contains("nobody")));
    // A password that breaks the rules signs nobody out.
    let (_, fresh) = devices("dora", 1).await;
    assert!(matches!(
        cli(
            &["user", "--config", c, "reset-password", "dora"],
            "short
"
        )
        .await,
        Err(CliError::Usage(_))
    ));
    assert!(meta.device_active(fresh[0]).await.unwrap());
}

/// `user invite` prints a join string and link to paste on the new computer (with `public_url` set), or the code
/// and how to finish it (without); a new one replaces the old; `--revoke` ends it.
#[tokio::test]
async fn user_invite_prints_a_join_string_and_revoke_ends_it() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();

    let out = cli(&["user", "--config", c, "invite", "cleo"], "").await.unwrap();
    assert!(out.contains("public_url"), "{out}");
    let code_line = out.lines().find_map(|l| l.strip_prefix("Code: ")).unwrap_or_else(|| panic!("{out}"));
    let bare = Code::parse(code_line, INVITE_CHARS).unwrap();

    // Top-level keys go before the first table.
    let text = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, format!("public_url = \"https://Cloud.Example.com/\"\n{text}")).unwrap();
    let out = cli(&["user", "invite", "--config", c, "cleo"], "").await.unwrap();
    let join = out.lines().find_map(|l| l.strip_prefix("Join string: ")).unwrap_or_else(|| panic!("{out}"));
    let link = out.lines().find_map(|l| l.strip_prefix("Link: ")).unwrap_or_else(|| panic!("{out}"));
    let parsed = JoinString::parse(join).unwrap();
    assert_eq!(parsed.server, "https://cloud.example.com");
    assert_eq!(JoinString::parse(link).unwrap(), parsed);
    assert!(link.starts_with("https://cloud.example.com/join/"), "{link}");
    assert!(out.contains("new account"), "{out}");

    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let now = craft_cloud_server::state::now_ms();
    assert_eq!(meta.invite_needs_password(&parsed.code.hash(), now).await.unwrap(), Some(true));
    assert_eq!(meta.invite_needs_password(&bare.hash(), now).await.unwrap(), None, "replaced");
    let ana = cli(&["user", "--config", c, "invite", "ana"], "").await.unwrap();
    assert!(!ana.contains("new account"), "{ana}");

    assert!(cli(&["user", "--config", c, "invite", "--revoke", "cleo"], "").await.unwrap().contains("revoked"));
    assert_eq!(meta.invite_needs_password(&parsed.code.hash(), now).await.unwrap(), None);
    assert!(matches!(cli(&["user", "--config", c, "invite", "--revoke", "cleo"], "").await, Err(CliError::Usage(_))), "nothing to revoke");
    assert!(matches!(cli(&["user", "--config", c, "invite", "bad name"], "").await, Err(CliError::Meta(_) | CliError::Usage(_))));

    std::fs::write(&config_path, format!("public_url = \"ftp://x\"\n{text}")).unwrap();
    assert!(matches!(Config::load(&config_path), Err(ConfigError::PublicUrl(_))));
}

#[tokio::test]
async fn init_validates_first_and_leaves_nothing_behind_on_failure() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let dir_s = dir.to_str().unwrap();

    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "bad name"], "admin password 1\n").await, Err(CliError::Usage(_))));
    assert!(!dir.exists());

    // Bad passwords are refused before anything is created.
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "ana"], "").await, Err(CliError::Usage(m)) if m.contains("no password")));
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "ana"], "\n").await, Err(CliError::Usage(_))));
    let long = format!("{}\n", "a".repeat(2000));
    assert!(matches!(cli(&["init", "--dir", dir_s, "--admin", "ana"], &long).await, Err(CliError::Usage(_))));
    assert!(!dir.exists());

    // A valid name in the same place succeeds.
    cli(&["init", "--dir", dir_s, "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert!(dir.join("config.toml").exists());
}

#[tokio::test]
async fn init_needs_behind_proxy_for_a_public_address() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let dir_s = dir.to_str().unwrap();

    let refused = cli(&["init", "--dir", dir_s, "--admin", "ana", "--listen", "0.0.0.0:8080"], "admin password 1\n").await;
    assert!(matches!(refused, Err(CliError::Usage(ref m)) if m.contains("--behind-proxy")), "{refused:?}");
    assert!(!dir.exists());

    // Behind a proxy, the proxy must be named: otherwise every client would share its address.
    let unnamed = cli(&["init", "--dir", dir_s, "--admin", "ana", "--listen", "0.0.0.0:8080", "--behind-proxy"], "admin password 1\n").await;
    assert!(matches!(unnamed, Err(CliError::Config(ConfigError::NoTrustedProxies))), "{unnamed:?}");
    assert!(!dir.exists());

    let args = ["init", "--dir", dir_s, "--admin", "ana", "--listen", "0.0.0.0:8080", "--behind-proxy", "--trusted-proxy", "10.0.0.1"];
    cli(&args, "admin password 1\n").await.unwrap();
    let config = Config::load(&dir.join("config.toml")).unwrap();
    assert!(config.behind_proxy);
    assert_eq!(config.trusted_proxies, vec!["10.0.0.1".parse::<std::net::IpAddr>().unwrap()]);
}

#[cfg(unix)]
#[tokio::test]
async fn init_makes_owner_only_files() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(dir.join("config.toml")).unwrap().permissions().mode() & 0o777, 0o600);
    let mode = |name: &str| std::fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("meta.db"), 0o600);
    for name in ["meta.db-wal", "meta.db-shm"] {
        if dir.join(name).exists() {
            assert_eq!(mode(name), 0o600, "{name}");
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn init_restricts_an_existing_empty_directory() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(dir.join("meta.db")).unwrap().permissions().mode() & 0o777, 0o600);
}

/// Kills the server process when the test ends, even if it panics.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_binary_serves_what_init_created() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let exe = env!("CARGO_BIN_EXE_craft-cloud-server");

    let mut init = Process::new(exe)
        .args(["init", "--dir", dir.to_str().unwrap(), "--admin", "ana", "--listen", &format!("127.0.0.1:{port}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    init.stdin.take().unwrap().write_all(b"admin password 1\n").unwrap();
    assert!(init.wait().unwrap().success());

    let server =
        Server(Process::new(exe).args(["serve", "--config", dir.join("config.toml").to_str().unwrap()]).stderr(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(20);
    let body = loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut conn) => {
                use std::io::Read;
                conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                conn.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                conn.write_all(b"GET /api/v1/info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
                let mut text = String::new();
                conn.read_to_string(&mut text).unwrap();
                break text;
            }
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => panic!("server never listened: {e}"),
        }
    };
    drop(server);
    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    assert!(body.contains("\"chunker_params\":[\"fastcdc-v1\"]"), "{body}");
}
