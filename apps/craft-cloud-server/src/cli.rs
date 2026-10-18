//! The `craft-cloud-server` command line. Passwords are read from standard input (one line), so they never
//! appear in the process list or shell history: `printf '%s\n' "$PASSWORD" | craft-cloud-server user add …`.

use std::io::{BufRead, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use craft_cloud_meta::{Meta, Role, valid_username};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::join::{Code, INVITE_CHARS, INVITE_TTL_MS, JoinString};
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::s3::S3Backend;

use crate::auth::{check_password_rules, hash_password};
use crate::config::{Config, StorageConfig};
use crate::state::now_ms;

#[derive(Debug, Parser)]
#[command(name = "craft-cloud-server", version, about = "Self-hosted sync and storage for the Craft App Suite")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a data directory with a config file, a SQLite database, blob storage and the first admin.
    Init {
        /// Directory to create (must not exist or be empty).
        #[arg(long)]
        dir: PathBuf,
        /// Username of the first admin. The password is read from standard input.
        #[arg(long)]
        admin: String,
        /// Address to listen on. Anything but loopback needs `--behind-proxy`.
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
        /// Set when a TLS reverse proxy forwards to the listen address. Needs `--trusted-proxy`.
        #[arg(long)]
        behind_proxy: bool,
        /// The reverse proxy's address, whose `X-Forwarded-For` is believed (repeat for several proxies).
        #[arg(long = "trusted-proxy")]
        trusted_proxies: Vec<IpAddr>,
    },
    /// Run the server.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Manage accounts.
    User {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: UserAction,
    },
    /// Manage a user's devices.
    Device {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Start a new epoch for a user's space (or every space with --all). Run after restoring the server from a backup.
    #[command(group(clap::ArgGroup::new("target").required(true).multiple(false).args(["username", "all"])))]
    ResetEpoch {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        /// The user whose space to reset.
        username: Option<String>,
        /// Reset every space on the server.
        #[arg(long)]
        all: bool,
    },
    /// Manage live documents.
    Live {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: LiveAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum LiveAction {
    /// Empty a user's recents document of an app (one that is full: `quota_exceeded`). Every machine then sends its
    /// whole document up again, so nothing any machine holds is lost.
    ResetDoc { username: String, app: String },
}

#[derive(Debug, Subcommand)]
pub enum UserAction {
    /// Add an account. The password is read from standard input.
    Add {
        username: String,
        #[arg(long)]
        admin: bool,
    },
    /// Set a new password, read from standard input.
    ResetPassword { username: String },
    /// Print an invite code for an account, good once for 7 days, to join a computer with in Craft Cloud. Creates the
    /// account (without a password, chosen when the invite is redeemed) if there is none, and replaces its earlier
    /// invite.
    Invite {
        username: String,
        /// Revoke the account's invite instead.
        #[arg(long)]
        revoke: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DeviceAction {
    List { username: String },
    Revoke { username: String, device: DeviceId },
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Meta(#[from] craft_cloud_meta::MetaError),
    #[error(transparent)]
    Store(#[from] craft_cloud_store::StoreError),
    #[error("{0}")]
    Usage(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Opens the database and blob storage a config names.
pub async fn open(config: &Config) -> Result<(Meta, Store), CliError> {
    let meta = Meta::open(&config.database.url).await?.with_stored_bytes_cap(config.stored_bytes_per_user);
    let store = match (&config.storage, config.storage.s3()?) {
        (_, Some(s3)) => Store::new(S3Backend::new(&s3)?),
        (StorageConfig::Disk { path }, None) => Store::new(DiskBackend::new(path)?),
        (StorageConfig::S3 { .. }, None) => return Err(CliError::Usage("S3 storage without settings".into())),
    };
    Ok((meta, store))
}

/// `--config` is global, which clap can't also make required, so it is checked here.
fn config_path(config: Option<PathBuf>) -> Result<PathBuf, CliError> {
    config.ok_or_else(|| CliError::Usage("--config <path to config.toml> is required".into()))
}

const USERNAME_RULE: &str = "usernames are 1-64 letters, digits, '.', '_' or '-'";

/// Reads one line, at most 1100 bytes: the 1024-byte password limit plus a line ending. Longer input is
/// refused by the password rules rather than buffered.
fn read_password(input: &mut impl BufRead) -> Result<String, CliError> {
    let mut line = String::new();
    input.take(1100).read_line(&mut line)?;
    if line.is_empty() {
        return Err(CliError::Usage("no password on standard input".into()));
    }
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    check_password_rules(&password).map_err(|e| CliError::Usage(e.message))?;
    Ok(password)
}

async fn user_id(meta: &Meta, username: &str) -> Result<craft_cloud_meta::UserRecord, CliError> {
    meta.user_by_name(username).await?.ok_or_else(|| CliError::Usage(format!("no user named {username}")))
}

/// Runs every command except `serve` (which `main` runs, since it never returns).
pub async fn run(command: Command, input: &mut impl BufRead, out: &mut impl Write) -> Result<(), CliError> {
    match command {
        Command::Init { dir, admin, listen, behind_proxy, trusted_proxies } => {
            init(&dir, &admin, &listen, (behind_proxy, &trusted_proxies), input, out).await
        }
        Command::Serve { .. } => Err(CliError::Usage("serve is run by main".into())),
        Command::User { config, action } => {
            let config = Config::load(&config_path(config)?)?;
            let (meta, _) = open(&config).await?;
            match action {
                UserAction::Invite { username, revoke: true } => {
                    if !meta.revoke_invite(&username).await? {
                        return Err(CliError::Usage(format!("{username} has no open invite")));
                    }
                    writeln!(out, "revoked the invite for {}", username.to_ascii_lowercase())?;
                }
                UserAction::Invite { username, revoke: false } => invite(&meta, &config, &username, out).await?,
                UserAction::Add { username, admin } => {
                    let password = read_password(input)?;
                    let role = if admin { Role::Admin } else { Role::Member };
                    let hash = hash_password(&password).map_err(|e| CliError::Usage(e.message))?;
                    let user = meta.create_user(&username, &hash, role, now_ms()).await?;
                    writeln!(out, "added {} ({})", user.username, if admin { "admin" } else { "member" })?;
                }
                UserAction::ResetPassword { username } => {
                    let user = user_id(&meta, &username).await?;
                    let password = read_password(input)?;
                    let hash = hash_password(&password).map_err(|e| CliError::Usage(e.message))?;
                    meta.set_password_hash(user.id, &hash).await?;
                    writeln!(out, "password changed for {}", user.username)?;
                }
            }
            Ok(())
        }
        Command::Device { config, action } => {
            let (meta, _) = open(&Config::load(&config_path(config)?)?).await?;
            match action {
                DeviceAction::List { username } => {
                    let user = user_id(&meta, &username).await?;
                    for d in meta.list_devices(user.id).await? {
                        writeln!(out, "{}  {}  last used {}{}", d.id, d.name, d.last_used_ms, if d.revoked { "  (revoked)" } else { "" })?;
                    }
                }
                DeviceAction::Revoke { username, device } => {
                    let user = user_id(&meta, &username).await?;
                    meta.revoke_device(user.id, device).await?;
                    writeln!(out, "revoked {device}")?;
                }
            }
            Ok(())
        }
        Command::ResetEpoch { config, username, all } => {
            let (meta, _) = open(&Config::load(&config_path(config)?)?).await?;
            if all {
                let spaces = meta.all_spaces().await?;
                for space in &spaces {
                    let epoch = meta.reset_epoch(*space).await?;
                    writeln!(out, "new epoch {epoch} for space {space}")?;
                }
                writeln!(out, "reset {} space(s)", spaces.len())?;
            } else {
                let username = username.ok_or_else(|| CliError::Usage("give a username, or --all".into()))?;
                let user = user_id(&meta, &username).await?;
                let epoch = meta.reset_epoch(user.space).await?;
                writeln!(out, "new epoch {epoch} for {}", user.username)?;
            }
            Ok(())
        }
        Command::Live { config, action: LiveAction::ResetDoc { username, app } } => {
            let (meta, _) = open(&Config::load(&config_path(config)?)?).await?;
            let user = user_id(&meta, &username).await?;
            let app = craft_cloud_proto::live::AppName::new(&app).map_err(|e| CliError::Usage(e.to_string()))?;
            let doc = craft_cloud_proto::live::recents_doc(user.space, &app);
            meta.reset_live_doc(user.space, doc).await?;
            writeln!(out, "emptied the {app} recents of {}; every machine sends its own again when it next connects", user.username)?;
            Ok(())
        }
    }
}

/// `user invite`: makes the invite and prints how to use it. The code is printed only here, never logged.
async fn invite(meta: &Meta, config: &Config, username: &str, out: &mut impl Write) -> Result<(), CliError> {
    let code = Code::generate(INVITE_CHARS).map_err(|e| CliError::Usage(format!("the system random source failed: {e}")))?;
    let made = meta.create_invite(username, &code.hash(), now_ms(), INVITE_TTL_MS).await?;
    // The username is the only personal identifier here: kept to debug, as a sign-in's is.
    tracing::debug!(user = %username.to_ascii_lowercase(), "invite created for");
    tracing::info!("invite created");
    let username = username.to_ascii_lowercase();
    if made.new_account {
        writeln!(out, "Invite for {username}, a new account: a password is chosen when the invite is used.")?;
    } else {
        writeln!(out, "Invite for {username}: it adds a computer to the account, without asking for its password.")?;
    }
    match &config.public_url {
        Some(server) => {
            let join = JoinString::new(server.clone(), code);
            writeln!(out, "Join string: {}", join.join())?;
            writeln!(out, "Link: {}", join.link())?;
            writeln!(out, "Enter it in Craft Cloud on the new computer. It works once, for 7 days.")?;
        }
        None => {
            writeln!(out, "Code: {}", code.display())?;
            writeln!(
                out,
                "Put the server's address in front of it, like cloud.example.com/{}, and enter that in Craft Cloud on the new \
                 computer. It works once, for 7 days. Set public_url in config.toml to have the whole join string printed.",
                code.display()
            )?;
        }
    }
    Ok(())
}

/// Creates a data directory. Every check that needs no disk access runs first, so a refused `init` leaves
/// nothing behind. Once the first file is made, any failure removes what this call made.
/// `proxy` is `--behind-proxy` and the `--trusted-proxy` addresses.
async fn init(
    dir: &Path,
    admin: &str,
    listen: &str,
    proxy: (bool, &[IpAddr]),
    input: &mut impl BufRead,
    out: &mut impl Write,
) -> Result<(), CliError> {
    let (behind_proxy, trusted_proxies) = proxy;
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        return Err(CliError::Usage(format!("{} is not empty", dir.display())));
    }
    let admin = admin.to_ascii_lowercase();
    if !valid_username(&admin) {
        return Err(CliError::Usage(USERNAME_RULE.into()));
    }
    let listen: SocketAddr = listen.parse().map_err(|_| CliError::Usage(format!("bad listen address {listen}")))?;
    if !behind_proxy && !listen.ip().is_loopback() {
        return Err(CliError::Usage(format!("{listen} is not a loopback address; pass --behind-proxy if a TLS reverse proxy forwards to it")));
    }
    let dir = std::path::absolute(dir)?;
    let db = without_verbatim_prefix(&dir.join("meta.db").display().to_string()).to_owned();
    let blobs = without_verbatim_prefix(&dir.join("blobs").display().to_string()).to_owned();
    let trusted: Vec<String> = trusted_proxies.iter().map(|ip| format!("\"{ip}\"")).collect();
    let config_text = format!(
        "listen = \"{listen}\"\nbehind_proxy = {behind_proxy}\ntrusted_proxies = [{}]\n\n[database]\nurl = {}\n\n[storage]\nkind = \"disk\"\npath = {}\n",
        trusted.join(", "),
        toml_string(&format!("sqlite:{db}")),
        toml_string(&blobs),
    );
    let config = Config::parse(&config_text)?;
    let password = read_password(input)?;
    let hash = hash_password(&password).map_err(|e| CliError::Usage(e.message))?;

    let created_dir = !dir.exists();
    if let Err(e) = create_files(&dir, &config_text, &config, &admin, &hash).await {
        remove_made(&dir, created_dir);
        return Err(e);
    }
    writeln!(out, "created {}\nrun: craft-cloud-server serve --config {}", dir.display(), dir.join("config.toml").display())?;
    Ok(())
}

async fn create_files(dir: &Path, config_text: &str, config: &Config, admin: &str, hash: &str) -> Result<(), CliError> {
    create_data_dir(dir)?;
    write_config(&dir.join("config.toml"), config_text)?;
    let (meta, _) = open(config).await?;
    meta.create_user(admin, hash, Role::Admin, now_ms()).await?;
    for name in ["meta.db", "meta.db-wal", "meta.db-shm"] {
        let path = dir.join(name);
        if path.exists() {
            restrict(&path, 0o600)?;
        }
    }
    Ok(())
}

/// Sets the file mode on Unix; other platforms have no equivalent here.
#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<(), CliError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<(), CliError> {
    Ok(())
}

/// Creates the directory (owner-only on Unix). Missing parents are created with the default mode.
fn create_data_dir(dir: &Path) -> Result<(), CliError> {
    if dir.exists() {
        // An empty directory the admin made keeps whatever mode they gave it; the data inside is private.
        restrict(dir, 0o700)?;
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    new_dir_builder().create(dir)?;
    Ok(())
}

#[cfg(unix)]
fn new_dir_builder() -> std::fs::DirBuilder {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder
}

#[cfg(not(unix))]
fn new_dir_builder() -> std::fs::DirBuilder {
    std::fs::DirBuilder::new()
}

/// Writes the config file, which holds the database URL and any S3 keys, so it is owner-only on Unix.
fn write_config(path: &Path, text: &str) -> Result<(), CliError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

/// Removes what a failed `init` made. The directory was empty (or absent) when `init` started, so
/// everything in it is `init`'s. Best effort: the original error is what the admin needs to see.
fn remove_made(dir: &Path, created_dir: bool) {
    for name in ["config.toml", "meta.db", "meta.db-wal", "meta.db-shm"] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    let _ = std::fs::remove_dir_all(dir.join("blobs"));
    if created_dir {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Drops the `\\?\` prefix Windows adds to canonical paths when it is followed by a drive letter, so the
/// path reads as a plain `C:\…` path in the config file and in the SQLite URL.
fn without_verbatim_prefix(path: &str) -> &str {
    match path.strip_prefix(r"\\?\") {
        Some(rest) if matches!(rest.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic()) => rest,
        _ => path,
    }
}

/// A TOML basic string (quotes and backslashes escaped), so Windows paths survive.
fn toml_string(s: &str) -> String {
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('"');
    for ch in s.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04X}", u32::from(c))),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_strings_escape_windows_paths() {
        assert_eq!(toml_string(r#"C:\data\"x""#), r#""C:\\data\\\"x\"""#);
    }

    #[test]
    fn verbatim_prefix_is_dropped_only_before_a_drive_letter() {
        assert_eq!(without_verbatim_prefix(r"\\?\C:\data\meta.db"), r"C:\data\meta.db");
        assert_eq!(without_verbatim_prefix(r"\\?\UNC\server\share"), r"\\?\UNC\server\share");
        assert_eq!(without_verbatim_prefix("/srv/data"), "/srv/data");
    }

    #[test]
    fn remove_made_clears_a_failed_init() {
        let parent = tempfile::tempdir().unwrap();
        let dir = parent.path().join("data");
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        for name in ["config.toml", "meta.db", "meta.db-wal", "blobs/chunk"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        remove_made(&dir, true);
        assert!(!dir.exists());
    }

    #[test]
    fn remove_made_keeps_a_directory_init_did_not_create() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::write(parent.path().join("config.toml"), b"x").unwrap();
        remove_made(parent.path(), false);
        assert!(parent.path().exists());
        assert!(!parent.path().join("config.toml").exists());
    }

    #[test]
    fn the_cli_parses() {
        let cli = Cli::try_parse_from(["craft-cloud-server", "user", "--config", "c.toml", "add", "ana", "--admin"]).unwrap();
        assert!(matches!(cli.command, Command::User { action: UserAction::Add { admin: true, .. }, .. }));
    }

    #[test]
    fn config_goes_before_or_after_the_action() {
        for args in [
            ["craft-cloud-server", "user", "--config", "c.toml", "add", "ana"],
            ["craft-cloud-server", "user", "add", "--config", "c.toml", "ana"],
            ["craft-cloud-server", "user", "add", "ana", "--config", "c.toml"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(
                matches!(&cli.command, Command::User { config, action: UserAction::Add { username, .. } } if config.as_deref() == Some(Path::new("c.toml")) && username == "ana")
            );
        }
        for args in [
            ["craft-cloud-server", "device", "--config", "c.toml", "list", "ana"],
            ["craft-cloud-server", "device", "list", "--config", "c.toml", "ana"],
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(&cli.command, Command::Device { config, .. } if config.as_deref() == Some(Path::new("c.toml"))));
        }
        for args in
            [["craft-cloud-server", "reset-epoch", "--config", "c.toml", "ana"], ["craft-cloud-server", "reset-epoch", "ana", "--config", "c.toml"]]
        {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(&cli.command, Command::ResetEpoch { config, .. } if config.as_deref() == Some(Path::new("c.toml"))));
        }
    }

    #[test]
    fn reset_epoch_takes_a_username_or_all_but_not_both_or_neither() {
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml", "ana"]).is_ok());
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml"]).is_err());
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml", "ana", "--all"]).is_err());
    }
}
