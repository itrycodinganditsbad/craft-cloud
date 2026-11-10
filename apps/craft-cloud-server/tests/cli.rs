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

/// The command line stays the way back in without HTTP: `user invite` replaces a password reset code the admin API made
/// (one row per account, so the reset code stops working), and makes an invite that adds a computer as before.
#[tokio::test]
async fn user_invite_replaces_a_password_reset_code() {
    use craft_cloud_meta::{AdminActor, AdminVia, RESET_TTL_MS};
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    let device = meta.create_device(ana.id, "pc", &[9u8; 32], 0).await.unwrap();
    let actor = AdminActor { user: ana.id, via: AdminVia::Device(device), password_hash: ana.password_hash.clone(), two_factor: false };
    let reset = Code::generate(INVITE_CHARS).unwrap();
    meta.admin_issue_reset(&actor, bob.id, &reset.hash(), false, 0, RESET_TTL_MS).await.unwrap();
    assert_eq!(meta.invite_needs_password(&reset.hash(), 1).await.unwrap(), Some(true));

    let out = cli(&["user", "--config", c, "invite", "bob"], "").await.unwrap();
    assert!(out.contains("Invite for bob") && out.contains("adds a computer"), "{out}");
    // The reset code is gone, and the new code is an invite: bob has a password, so it asks for none.
    assert_eq!(meta.invite_needs_password(&reset.hash(), 1).await.unwrap(), None);
    let code = out.lines().find_map(|l| l.strip_prefix("Code: ")).unwrap().trim().to_owned();
    let invite = Code::parse_any(&code).unwrap();
    assert_eq!(meta.invite_needs_password(&invite.hash(), 1).await.unwrap(), Some(false));
    // The command line does not enforce what the HTTP API does: it can reset the last admin's password.
    assert!(cli(&["user", "--config", c, "reset-password", "ana"], "a brand new password\n").await.unwrap().contains("password changed for ana"));
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// A failed sign-in's event `days` days ago, written through the store a config opens, for the pruning to find.
async fn failure_days_ago(config: &Config, days: i64, target: Option<craft_cloud_meta::UserId>) {
    use craft_cloud_meta::{AuditAction, AuditFailure, Step};
    // Opened without the retention, which would prune the older events when a later one is written: the events are dated
    // in the past here, and the server's clock is not.
    let meta = craft_cloud_meta::Meta::open(&config.database.url).await.unwrap();
    let failure = AuditFailure { action: AuditAction::AuthLoginFailed, target, step: Step::Password };
    meta.audit_record_failure(failure, craft_cloud_server::state::now_ms() - days * DAY_MS).await.unwrap();
}

/// The command line's recovery actions write their `cli.*` events, with no account acting, and `audit list` shows them as
/// text, newest first, with IDs and no names.
#[tokio::test]
async fn the_recovery_commands_write_events_and_audit_list_shows_them() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    cli(&["user", "--config", c, "add", "bob"], "bob password 1\n").await.unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&config_path).unwrap()).await.unwrap();
    let bob = meta.user_by_name("bob").await.unwrap().unwrap();
    // Adding an account is not one of the events.
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("0 event(s)\n"));

    let first = meta.create_device(bob.id, "pc", &[1; 32], 0).await.unwrap();
    let _second = meta.create_device(bob.id, "laptop", &[2; 32], 0).await.unwrap();
    cli(&["user", "--config", c, "reset-password", "bob", "--keep-devices"], "a brand new password\n").await.unwrap();
    cli(&["device", "--config", c, "revoke", "bob", &first.to_string()], "").await.unwrap();
    // The same again changes nothing, and so writes nothing.
    cli(&["device", "--config", c, "revoke", "bob", &first.to_string()], "").await.unwrap();
    meta.create_device(bob.id, "third", &[3; 32], 0).await.unwrap();
    cli(&["device", "--config", c, "revoke-all", "bob"], "").await.unwrap();
    cli(&["device", "--config", c, "revoke-all", "bob"], "").await.unwrap();

    let events = meta.audit_list(&craft_cloud_meta::AuditQuery { limit: 100, ..Default::default() }).await.unwrap();
    let mut names: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, ["cli.device_revoked", "cli.devices_revoked_all", "cli.password_reset"]);
    for e in &events {
        assert_eq!(
            (e.actor_user, e.actor_device, e.target_id),
            (None, None, Some(bob.id.0)),
            "the command line is no account: {}",
            e.action.as_str()
        );
    }

    let out = cli(&["audit", "--config", c, "list"], "").await.unwrap();
    assert!(out.ends_with("3 event(s)\n"), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].contains("cli.devices_revoked_all") && lines[0].contains("success") && lines[0].contains("actor=-  device=-"), "{out}");
    assert!(lines[0].contains(&format!("target=user:{}", bob.id.0)) && lines[0].contains(r#"{"revoked":2}"#), "{out}");
    assert!(lines[2].contains("cli.password_reset") && lines[2].contains(r#"{"revoked":0}"#), "{out}");
    // Accounts are named by ID: no name, no device name, no password in the text.
    for secret in ["bob", "ana", "laptop", "third", "$argon2"] {
        assert!(!out.contains(secret), "{secret} in {out}");
    }
    // Filters, on a name or on a group of names, on an account, and a limit.
    assert!(cli(&["audit", "--config", c, "list", "--action", "cli."], "").await.unwrap().ends_with("3 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--action", "cli.password_reset"], "").await.unwrap().ends_with("1 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--action", "auth."], "").await.unwrap().ends_with("0 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--user", "Bob"], "").await.unwrap().ends_with("3 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--user", "ana"], "").await.unwrap().ends_with("0 event(s)\n"));
    assert!(cli(&["audit", "--config", c, "list", "--limit", "2"], "").await.unwrap().ends_with("2 event(s)\n"));
    // Bad input is a usage error.
    for bad in [&["--limit", "0"][..], &["--limit", "201"], &["--action", "bogus"], &["--action", "cli"], &["--user", "nobody"]] {
        let mut args = vec!["audit", "--config", c, "list"];
        args.extend_from_slice(bad);
        assert!(matches!(cli(&args, "").await, Err(CliError::Usage(_))), "{bad:?}");
    }
    assert!(matches!(cli(&["audit", "list"], "").await, Err(CliError::Usage(m)) if m.contains("--config")));
}

/// `audit prune` deletes the events older than `[audit] retention_days` (365 without the section) and reports how many;
/// `--dry-run` only counts.
#[tokio::test]
async fn audit_prune_deletes_what_is_past_the_retention_and_dry_run_only_counts() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    // Without the section: a year.
    assert_eq!(Config::load(&config_path).unwrap().audit.retention_days, 365);
    for days in [400, 399, 300, 1] {
        failure_days_ago(&Config::load(&config_path).unwrap(), days, None).await;
    }
    let dry = cli(&["audit", "--config", c, "prune", "--dry-run"], "").await.unwrap();
    assert_eq!(dry, "2 audit event(s) are older than 365 days; dry run, nothing deleted\n");
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("4 event(s)\n"), "a dry run deletes nothing");
    assert_eq!(cli(&["audit", "--config", c, "prune"], "").await.unwrap(), "deleted 2 audit event(s) older than 365 days\n");
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("2 event(s)\n"));
    assert_eq!(cli(&["audit", "--config", c, "prune"], "").await.unwrap(), "deleted 0 audit event(s) older than 365 days\n");

    // With the section: a month.
    let text = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, format!("{text}\n[audit]\nretention_days = 30\n")).unwrap();
    assert_eq!(Config::load(&config_path).unwrap().audit.retention_days, 30);
    failure_days_ago(&Config::load(&config_path).unwrap(), 31, None).await;
    assert_eq!(
        cli(&["audit", "--config", c, "prune"], "").await.unwrap(),
        "deleted 2 audit event(s) older than 30 days\n",
        "the one of 300 days ago and the one of 31"
    );
    assert!(cli(&["audit", "--config", c, "list"], "").await.unwrap().ends_with("1 event(s)\n"), "the one of yesterday stays");
}

/// `[audit]` is read or refused as the design says: absent it is the default, a value out of range or an unknown key is an
/// error that names the problem and not the file's text.
#[tokio::test]
async fn the_audit_config_section_is_optional_and_checked() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let base = std::fs::read_to_string(&config_path).unwrap();
    let with = |section: &str| Config::parse(&format!("{base}\n{section}\n"));
    assert_eq!(with("").unwrap().audit.retention_days, 365);
    assert_eq!(with("[audit]").unwrap().audit.retention_days, 365);
    assert_eq!(with("[audit]\nretention_days = 1").unwrap().audit.retention_days, 1);
    assert_eq!(with("[audit]\nretention_days = 3650").unwrap().audit.retention_days, 3650);
    assert!(matches!(with("[audit]\nretention_days = 0"), Err(ConfigError::AuditRetention(0, 1, 3650))));
    assert!(matches!(with("[audit]\nretention_days = 3651"), Err(ConfigError::AuditRetention(3651, 1, 3650))));
    assert!(matches!(with("[audit]\nretention_days = -1"), Err(ConfigError::Parse(_))));
    assert!(matches!(with("[audit]\nretention = 30"), Err(ConfigError::Parse(_))), "an unknown key is refused");
    let shown = with("[audit]\nretention_days = 9999").unwrap_err().to_string();
    assert!(shown.contains("9999") && shown.contains("3650"), "{shown}");
}

// ---- two-factor sign-in: the key file, `serve`, `user reset-2fa` and `two-factor rekey` ----

use craft_cloud_meta::{DeviceActor, Meta, MetaError, RevokeReason, UserId};
use craft_cloud_server::secretbox::{KEY_BYTES, SecretBox, TwoFactorKey};
use craft_cloud_server::two_factor::{KeyState, check_startup, load_keys, rekey_hooked};

/// A data directory made by `init` (admin `ana`) and its database.
async fn init_dir(parent: &std::path::Path) -> (std::path::PathBuf, Meta) {
    let dir = parent.join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let (meta, _) = craft_cloud_server::cli::open(&Config::load(&dir.join("config.toml")).unwrap()).await.unwrap();
    (dir, meta)
}

/// Turns two-factor sign-in on for `name` (made if needed) with `sealed` as its secret; returns the device that did it.
async fn protect(meta: &Meta, name: &str, sealed: &[u8]) -> (craft_cloud_meta::UserRecord, craft_cloud_proto::DeviceId) {
    let user = match meta.user_by_name(name).await.unwrap() {
        Some(user) => user,
        None => meta.create_user(name, "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap(),
    };
    let device = meta.create_device(user.id, "pc", blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), 0).await.unwrap();
    let actor = DeviceActor { user: user.id, device, password_hash: user.password_hash.clone() };
    meta.two_factor_begin_setup(&actor, sealed, 1_000).await.unwrap();
    let hashes: Vec<[u8; 32]> = (0..10).map(|_| Code::generate(INVITE_CHARS).unwrap().hash()).collect();
    meta.two_factor_confirm(user.id, device, &|_: UserId, _: &[u8]| Ok(Some(5)), &hashes, 2_000).await.unwrap();
    (user, device)
}

#[tokio::test]
async fn init_makes_the_two_factor_key_and_names_it_in_the_config() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let out = cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert!(out.contains("two-factor.key") && out.contains("apart from your database backups"), "{out}");
    let key = std::fs::read(dir.join("two-factor.key")).unwrap();
    assert_eq!(key.len(), KEY_BYTES);
    assert_ne!(key, vec![0u8; KEY_BYTES]);
    let config = Config::load(&dir.join("config.toml")).unwrap();
    let section = config.two_factor.clone().unwrap();
    assert_eq!(section.key_file, std::path::PathBuf::from("two-factor.key"));
    assert_eq!(config.key_path(&section.key_file), dir.join("two-factor.key"));
    assert!(matches!(load_keys(&config).unwrap(), KeyState::Loaded(_)));
    // The key is in no log line of init and not in the config.
    let text = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(text.contains("[two_factor]") && text.contains("key_file = \"two-factor.key\""), "{text}");
    // Two directories, two keys.
    let other = parent.path().join("other");
    cli(&["init", "--dir", other.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_ne!(std::fs::read(other.join("two-factor.key")).unwrap(), key);
}

#[cfg(unix)]
#[tokio::test]
async fn the_two_factor_key_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    assert_eq!(std::fs::metadata(dir.join("two-factor.key")).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn a_config_from_before_two_factor_sign_in_loads_and_has_no_key() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let path = dir.join("config.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, text.split("[two_factor]").next().unwrap()).unwrap();
    let config = Config::load(&path).unwrap();
    assert_eq!(config.two_factor, None);
    assert!(matches!(load_keys(&config).unwrap(), KeyState::NotConfigured));
    // No key and no secrets: the server runs, and two-factor sign-in can't be set up.
    assert!(check_startup(&meta, KeyState::NotConfigured, &config, 0).await.unwrap().is_none());
    // An unknown key in the section is refused.
    std::fs::write(&path, format!("{text}key_pile = 1\n")).unwrap();
    assert!(matches!(Config::load(&path), Err(ConfigError::Parse(_))));
}

/// The server fails closed: it will not start on a database whose two-factor secrets its key can't open, and it opens every one
/// of them to know. The message names the key file the config expects.
#[tokio::test]
async fn the_server_refuses_to_start_when_the_key_cannot_open_every_secret() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let config = Config::load(&dir.join("config.toml")).unwrap();
    let key_path = dir.join("two-factor.key");
    let KeyState::Loaded(secret_box) = load_keys(&config).unwrap() else { panic!("a key") };
    let (ana, bob, cy) = (meta.user_by_name("ana").await.unwrap().unwrap(), protect(&meta, "bob", b"x").await.0, protect(&meta, "cy", b"x").await.0);
    let _ = (ana, bob);
    // Nothing sealed with the key yet: every secret is a stranger's.
    let err = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap_err();
    assert!(err.contains("2 of 2") && err.contains(&key_path.display().to_string()), "{err}");
    // Re-seal both properly, then break one: all are opened, so one is enough to refuse.
    for name in ["bob", "cy"] {
        let user = meta.user_by_name(name).await.unwrap().unwrap();
        let sealed = secret_box.seal(user.id, b"12345678901234567890").unwrap();
        assert!(meta.two_factor_replace_ciphertext(user.id, false, b"x", &sealed).await.unwrap());
    }
    let opened = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap();
    assert!(opened.is_some(), "the right key opens them all");
    let sealed_for_someone_else = secret_box.seal(UserId(uuid::Uuid::now_v7()), b"12345678901234567890").unwrap();
    let current = meta.two_factor_secrets(5_000).await.unwrap().into_iter().find(|s| s.user == cy.id).unwrap().active.unwrap();
    assert!(meta.two_factor_replace_ciphertext(cy.id, false, &current, &sealed_for_someone_else).await.unwrap());
    let err = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap_err();
    assert!(err.contains("1 of 2") && err.contains(&key_path.display().to_string()), "{err}");
    assert!(!err.contains("12345678901234567890"), "no secret in the message");

    // A different key at the same path: the same.
    std::fs::write(&key_path, [7u8; KEY_BYTES]).unwrap();
    let err = check_startup(&meta, load_keys(&config).unwrap(), &config, 5_000).await.unwrap_err();
    assert!(err.contains("2 of 2"), "{err}");
    // No key file at all, with secrets: refused, naming the path (the server never makes one).
    std::fs::remove_file(&key_path).unwrap();
    let keys = load_keys(&config).unwrap();
    assert!(matches!(keys, KeyState::Missing(ref p) if *p == key_path), "{keys:?}");
    let err = check_startup(&meta, keys, &config, 5_000).await.unwrap_err();
    assert!(err.contains(&key_path.display().to_string()) && err.contains("does not exist"), "{err}");
    assert!(!key_path.exists(), "nothing made it");
    // A key file of the wrong length is an error of its own, naming the file.
    std::fs::write(&key_path, [7u8; 31]).unwrap();
    assert!(load_keys(&config).unwrap_err().to_string().contains("exactly 32 bytes"));
    // No `[two_factor]` section at all, with secrets: refused too.
    let mut bare = config.clone();
    bare.two_factor = None;
    let err = check_startup(&meta, KeyState::NotConfigured, &bare, 5_000).await.unwrap_err();
    assert!(err.contains("key_file"), "{err}");
}

#[tokio::test]
async fn without_secrets_a_missing_key_does_not_stop_the_server() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let config = Config::load(&dir.join("config.toml")).unwrap();
    std::fs::remove_file(dir.join("two-factor.key")).unwrap();
    let keys = load_keys(&config).unwrap();
    assert!(matches!(keys, KeyState::Missing(_)));
    assert!(check_startup(&meta, keys, &config, 0).await.unwrap().is_none(), "it runs; set-up answers two_factor_unavailable");
    assert!(!dir.join("two-factor.key").exists());
}

#[tokio::test]
async fn reset_2fa_takes_it_away_and_signs_every_device_out() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let c = dir.join("config.toml");
    let c = c.to_str().unwrap();
    let (bob, device) = protect(&meta, "bob", b"sealed").await;
    let extra = meta.create_device(bob.id, "second", &[8u8; 32], 0).await.unwrap();
    // Wrong codes lock it; the reset lifts the lock with the rest.
    for _ in 0..10 {
        let wrong = |_: UserId, _: &[u8]| -> Result<Option<i64>, craft_cloud_meta::UnreadableSecret> { Ok(None) };
        let _ = meta.confirm_second_factor(bob.id, Some(&craft_cloud_meta::Proof::Totp(&wrong)), 3_000).await;
    }
    assert!(meta.two_factor_locked_for(bob.id, 3_001).await.unwrap().is_some());
    let out = cli(&["user", "--config", c, "reset-2fa", "Bob"], "").await.unwrap();
    assert!(out.contains("two-factor sign-in of bob is off") && out.contains("signed out 2 device(s)"), "{out}");
    assert!(!meta.two_factor_enabled(bob.id).await.unwrap());
    assert_eq!(meta.two_factor_locked_for(bob.id, 3_001).await.unwrap(), None);
    for id in [device, extra] {
        assert!(!meta.device_active(id).await.unwrap());
    }
    assert!(meta.list_devices(bob.id).await.unwrap().iter().all(|d| d.revoke_reason == Some(RevokeReason::TwoFactor)));
    // Again: nothing to reset, and nothing is signed out. An unknown user is an error, not a panic.
    let fresh = meta.create_device(bob.id, "fresh", &[9u8; 32], 0).await.unwrap();
    assert!(matches!(cli(&["user", "--config", c, "reset-2fa", "bob"], "").await, Err(CliError::Meta(MetaError::Conflict(_)))));
    assert!(meta.device_active(fresh).await.unwrap());
    assert!(matches!(cli(&["user", "--config", c, "reset-2fa", "nobody"], "").await, Err(CliError::Usage(m)) if m.contains("nobody")));
    // `--config` after the action, as everywhere.
    protect(&meta, "bob", b"sealed").await;
    assert!(cli(&["user", "reset-2fa", "--config", c, "bob"], "").await.unwrap().contains("is off"));
}

/// `two-factor rekey`: every secret under the older key is rewritten under the current one; it can be run again, refuses
/// (changing nothing) when a secret has a key that is neither, and after it the older key can go.
#[tokio::test]
async fn rekey_moves_every_secret_to_the_new_key_and_is_repeatable() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let c = dir.join("config.toml");
    let (old_path, new_path) = (dir.join("two-factor.key"), dir.join("new.key"));
    let old = TwoFactorKey::from_file(&old_path).unwrap();
    let old_box = SecretBox::new(&old, None).unwrap();
    let mut users = Vec::new();
    for name in ["bob", "cy"] {
        let user = meta.create_user(name, "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
        users.push(protect(&meta, name, &old_box.seal(user.id, b"12345678901234567890").unwrap()).await.0);
    }
    // A set-up that is open is rewritten as well.
    let dora = meta.create_user("dora", "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
    let dev = meta.create_device(dora.id, "pc", &[4u8; 32], 0).await.unwrap();
    let actor = DeviceActor { user: dora.id, device: dev, password_hash: "$argon2id$stub".to_owned() };
    let now = craft_cloud_server::state::now_ms();
    meta.two_factor_begin_setup(&actor, &old_box.seal(dora.id, b"pending-pending-pend").unwrap(), now).await.unwrap();

    // Rotating: the new key in `key_file`, the old one in `key_file_old`.
    let new_key = loop {
        let key = TwoFactorKey::generate().unwrap();
        if key.id() != old.id() {
            break key;
        }
    };
    std::fs::write(&new_path, new_key.to_file_bytes().as_slice()).unwrap();
    let text = std::fs::read_to_string(&c).unwrap();
    let rotated = text.replace("key_file = \"two-factor.key\"", "key_file = \"new.key\"\nkey_file_old = \"two-factor.key\"");
    assert_ne!(rotated, text);
    std::fs::write(&c, &rotated).unwrap();
    let cs = c.to_str().unwrap();
    // The server starts in the middle of a rotation: both keys open everything.
    let config = Config::load(&c).unwrap();
    assert!(check_startup(&meta, load_keys(&config).unwrap(), &config, now).await.unwrap().is_some());

    let out = cli(&["two-factor", "--config", cs, "rekey"], "").await.unwrap();
    assert!(
        out.contains("rewrote 3 secret(s) under the current key; 0 were under it already")
            && out.contains("every secret is under the current key now")
            && out.contains("key_file_old can be removed once every server instance"),
        "{out}"
    );
    let new_box = SecretBox::new(&new_key, None).unwrap();
    for user in &users {
        let sealed = meta.two_factor_secrets(now).await.unwrap().into_iter().find(|s| s.user == user.id).unwrap().active.unwrap();
        assert_eq!(new_box.open(user.id, &sealed).unwrap().as_slice(), b"12345678901234567890", "only the new key is needed now");
        assert_eq!(SecretBox::key_id(&sealed), Some(new_key.id()));
    }
    let pending = meta.two_factor_secrets(now).await.unwrap().into_iter().find(|s| s.user == dora.id).unwrap().pending.unwrap();
    assert_eq!(new_box.open(dora.id, &pending).unwrap().as_slice(), b"pending-pending-pend");
    // Run again: nothing left to do. Then the old key goes, and the server starts on the new one alone.
    let out = cli(&["two-factor", "--config", cs, "rekey"], "").await.unwrap();
    assert!(out.contains("rewrote 0 secret(s) under the current key; 3 were under it already"), "{out}");
    std::fs::write(&c, text.replace("key_file = \"two-factor.key\"", "key_file = \"new.key\"")).unwrap();
    std::fs::remove_file(&old_path).unwrap();
    let config = Config::load(&c).unwrap();
    assert!(check_startup(&meta, load_keys(&config).unwrap(), &config, now).await.unwrap().is_some());

    // A secret under a key that is neither refuses the whole rotation and changes nothing.
    let stranger = SecretBox::new(&key_with_other_name(&[old.id(), new_key.id()]), None).unwrap();
    let eve = meta.create_user("eve", "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
    protect(&meta, "eve", &stranger.seal(eve.id, b"12345678901234567890").unwrap()).await;
    let before = meta.two_factor_secrets(now).await.unwrap();
    std::fs::write(&c, rotated).unwrap();
    std::fs::write(&old_path, old.to_file_bytes().as_slice()).unwrap();
    let refused = cli(&["two-factor", "--config", cs, "rekey"], "").await;
    assert!(matches!(refused, Err(CliError::Usage(ref m)) if m.contains("neither") && m.contains("nothing was changed")), "{refused:?}");
    assert_eq!(meta.two_factor_secrets(now).await.unwrap(), before);
    // And the server doesn't start with it either.
    let config = Config::load(&c).unwrap();
    assert!(check_startup(&meta, load_keys(&config).unwrap(), &config, now).await.is_err());
    // Without any key configured, rekey says so.
    std::fs::write(&c, text.split("[two_factor]").next().unwrap()).unwrap();
    assert!(matches!(cli(&["two-factor", "--config", cs, "rekey"], "").await, Err(CliError::Usage(m)) if m.contains("no key")));
}

/// A key whose name is none of `taken`.
fn key_with_other_name(taken: &[u8]) -> TwoFactorKey {
    loop {
        let key = TwoFactorKey::generate().unwrap();
        if !taken.contains(&key.id()) {
            return key;
        }
    }
}

fn spawn_serve(exe: &str, config: &std::path::Path) -> Child {
    Process::new(exe).args(["serve", "--config", config.to_str().unwrap()]).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().unwrap()
}

fn init_with_binary(exe: &str, dir: &std::path::Path, port: u16) {
    use std::io::Write;
    let mut init = Process::new(exe)
        .args(["init", "--dir", dir.to_str().unwrap(), "--admin", "ana", "--listen", &format!("127.0.0.1:{port}")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    init.stdin.take().unwrap().write_all(b"admin password 1\n").unwrap();
    assert!(init.wait().unwrap().success());
}

fn listens(port: u16, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `serve` never makes the key. Without one and without secrets it runs; with secrets in the database it exits with an error
/// that names the key file, and listens on nothing.
#[test]
fn serve_never_makes_the_key_and_refuses_secrets_it_cannot_open() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let exe = env!("CARGO_BIN_EXE_craft-cloud-server");
    init_with_binary(exe, &dir, port);
    let key_path = dir.join("two-factor.key");
    assert!(key_path.exists());

    // No key file, no secrets: it runs, and the file is still not there.
    std::fs::remove_file(&key_path).unwrap();
    let running = Server(spawn_serve(exe, &dir.join("config.toml")));
    assert!(listens(port, Duration::from_secs(20)), "the server never listened");
    drop(running);
    assert!(!key_path.exists(), "serve must not make the key");

    // A secret in the database and no key: it exits at once with the path in its message, and listens on nothing.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let (meta, _) = craft_cloud_server::cli::open(&Config::load(&dir.join("config.toml")).unwrap()).await.unwrap();
        protect(&meta, "bob", b"a sealed secret that no key here can open").await;
    });
    let mut refused = spawn_serve(exe, &dir.join("config.toml"));
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        match refused.try_wait().unwrap() {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            None => {
                let _ = refused.kill();
                panic!("the server kept running on secrets it cannot open");
            }
        }
    };
    assert!(!status.success());
    let mut message = String::new();
    std::io::Read::read_to_string(&mut refused.stderr.take().unwrap(), &mut message).unwrap();
    assert!(message.contains("error:") && message.contains(&key_path.display().to_string()), "{message}");
    assert!(!message.contains("a sealed secret"), "no secret in the message");
    assert!(!key_path.exists(), "serve must not make the key");
    assert!(!listens(port, Duration::from_millis(300)));

    // With a key that doesn't open the secret: refused as well.
    std::fs::write(&key_path, [3u8; KEY_BYTES]).unwrap();
    let refused = Process::new(exe).args(["serve", "--config", dir.join("config.toml").to_str().unwrap()]).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("can't be opened"), "{}", String::from_utf8_lossy(&refused.stderr));
}

/// A confirmation that lands exactly between `two-factor rekey`'s first scan and its rewrite can't leave a secret under the old
/// key. The seam (`rekey_hooked`) holds the rekey after its scan until the confirmations, which move each set-up's secret to the
/// active one, have all committed. Rekey reads an account's secrets fresh under its lock; a rewrite from the scan's snapshot would
/// find its pending ciphertext gone and leave the one now active under the old key. The first successful rekey is the one that
/// is checked: no second run repairs anything here.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_confirmation_between_rekeys_scan_and_rewrite_leaves_nothing_under_the_old_key() {
    let parent = tempfile::tempdir().unwrap();
    let (dir, meta) = init_dir(parent.path()).await;
    let old = TwoFactorKey::from_file(&dir.join("two-factor.key")).unwrap();
    let old_box = SecretBox::new(&old, None).unwrap();
    let new_key = key_with_other_name(&[old.id()]);
    let now = craft_cloud_server::state::now_ms();
    for round in 0..4 {
        let mut confirms = Vec::new();
        for n in 0..6 {
            let user = meta.create_user(&format!("u{round}x{n}"), "$argon2id$stub", craft_cloud_meta::Role::Member, 0).await.unwrap();
            let device = meta.create_device(user.id, "pc", blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), 0).await.unwrap();
            let actor = DeviceActor { user: user.id, device, password_hash: "$argon2id$stub".to_owned() };
            meta.two_factor_begin_setup(&actor, &old_box.seal(user.id, b"12345678901234567890").unwrap(), now).await.unwrap();
            confirms.push((user.id, device));
        }
        let (scanned_tx, scanned_rx) = std::sync::mpsc::channel::<()>();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let rekeying = {
            let meta = meta.clone();
            let both = SecretBox::new(&new_key, Some(&old)).unwrap();
            tokio::spawn(async move {
                let go_rx = std::sync::Mutex::new(go_rx);
                let hold = move || {
                    let _ = scanned_tx.send(());
                    let _ = go_rx.lock().unwrap().recv_timeout(Duration::from_secs(30));
                };
                rekey_hooked(&meta, &both, &hold).await
            })
        };
        // Rekey has scanned (its snapshot holds every set-up's secret as a pending one under the old key)…
        tokio::task::spawn_blocking(move || scanned_rx.recv_timeout(Duration::from_secs(30))).await.unwrap().expect("rekey scanned");
        // …now every confirmation commits, moving the pending secret to the active one…
        for (user, device) in confirms {
            let hashes: Vec<[u8; 32]> = (0..10).map(|_| Code::generate(INVITE_CHARS).unwrap().hash()).collect();
            meta.two_factor_confirm(user, device, &|_: UserId, _: &[u8]| Ok(Some(5)), &hashes, now + 1).await.unwrap();
        }
        assert!(
            meta.two_factor_all_secrets().await.unwrap().iter().all(|row| row.pending.is_none() && row.active.is_some()),
            "the set-ups are active now"
        );
        // …and only then does rekey go on.
        go_tx.send(()).unwrap();
        let done = rekeying.await.unwrap().expect("rekey succeeds");
        assert_eq!(done.rewritten, 6, "round {round}: the secrets that became active meanwhile were rewritten");
        for row in meta.two_factor_all_secrets().await.unwrap() {
            for sealed in [row.active, row.pending].into_iter().flatten() {
                assert_eq!(SecretBox::key_id(&sealed), Some(new_key.id()), "round {round}: a secret is still under the old key after one rekey");
            }
        }
    }
    let alone = SecretBox::new(&new_key, None).unwrap();
    for row in meta.two_factor_all_secrets().await.unwrap() {
        for sealed in [row.active, row.pending].into_iter().flatten() {
            alone.open(row.user, &sealed).unwrap();
        }
    }
}

#[tokio::test]
async fn web_sessions_prune_deletes_only_what_is_over() {
    use craft_cloud_meta::{NewChallenge, NewWebSession, WebSignIn};
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data");
    cli(&["init", "--dir", dir.to_str().unwrap(), "--admin", "ana"], "admin password 1\n").await.unwrap();
    let config_path = dir.join("config.toml");
    let c = config_path.to_str().unwrap();
    let config = Config::load(&config_path).unwrap();
    let idle = config.web_admin.idle_ms();
    let now = craft_cloud_server::state::now_ms();

    // One session per way of being over, and one that is not. (The sign-in checks the generation it is given.)
    let (meta, _) = craft_cloud_server::cli::open(&config).await.unwrap();
    let ana = meta.user_by_name("ana").await.unwrap().unwrap();
    let sign_in = |at: i64, absolute_ms: i64| {
        let (meta, ana) = (meta.clone(), ana.clone());
        async move {
            let session = NewWebSession { id_hash: *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), absolute_ms };
            let challenge =
                NewChallenge { id_hash: *blake3::hash(uuid::Uuid::now_v7().as_bytes()).as_bytes(), client_key: "ip:127.0.0.1".to_owned() };
            let made = meta.web_sign_in(ana.id, (&ana.password_hash, ana.auth_gen), &challenge, &session, at).await.unwrap();
            assert_eq!(made, Some(WebSignIn::Session));
            session
        }
    };
    let day = 24 * 3_600_000;
    let idle_one = sign_in(now - idle - 1_000, 30 * day).await;
    let expired_one = sign_in(now - 2 * day, day).await;
    let logged_out = sign_in(now - 1_000, day).await;
    meta.web_logout(&logged_out.id_hash, now - 500).await.unwrap();
    let valid = sign_in(now - 1_000, day).await;

    let out = cli(&["web-sessions", "--config", c, "prune", "--dry-run"], "").await.unwrap();
    assert_eq!(out, "3 web session(s) are over; dry run, nothing deleted\n");
    assert!(meta.web_sessions_count_over(now, idle).await.unwrap() == 3, "a dry run deletes nothing");
    let out = cli(&["web-sessions", "prune", "--config", c], "").await.unwrap();
    assert_eq!(out, "deleted 3 web session(s) that were over\n");
    assert_eq!(cli(&["web-sessions", "--config", c, "prune"], "").await.unwrap(), "deleted 0 web session(s) that were over\n");
    assert!(meta.web_session(&valid.id_hash, now, idle).await.unwrap().is_some(), "a valid session is never touched");
    for gone in [&idle_one, &expired_one, &logged_out] {
        assert!(meta.web_session(&gone.id_hash, now, idle).await.unwrap().is_none());
    }
    assert!(matches!(cli(&["web-sessions", "prune"], "").await, Err(CliError::Usage(m)) if m.contains("--config")));
}
