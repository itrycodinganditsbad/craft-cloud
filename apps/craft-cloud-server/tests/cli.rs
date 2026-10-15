//! The admin command line, called in-process, and the real binary started with `init` + `serve`.

use std::io::Cursor;
use std::process::{Child, Command as Process, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
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
