//! The `craft-cloud-server` command line. Passwords are read from standard input (one line), so they never
//! appear in the process list or shell history: `printf '%s\n' "$PASSWORD" | craft-cloud-server user add …`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use craft_cloud_meta::{Meta, Role};
use craft_cloud_proto::DeviceId;
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
        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:8080")]
        listen: String,
    },
    /// Run the server.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Manage accounts.
    User {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: UserAction,
    },
    /// Manage a user's devices.
    Device {
        #[arg(long)]
        config: PathBuf,
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Start a new epoch for a user's space. Run after restoring the server from a backup.
    ResetEpoch {
        #[arg(long)]
        config: PathBuf,
        username: String,
    },
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
    let meta = Meta::open(&config.database.url).await?;
    let store = match (&config.storage, config.storage.s3()?) {
        (_, Some(s3)) => Store::new(S3Backend::new(&s3)?),
        (StorageConfig::Disk { path }, None) => Store::new(DiskBackend::new(path)?),
        (StorageConfig::S3 { .. }, None) => return Err(CliError::Usage("S3 storage without settings".into())),
    };
    Ok((meta, store))
}

fn read_password(input: &mut impl BufRead) -> Result<String, CliError> {
    let mut line = String::new();
    input.read_line(&mut line)?;
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
        Command::Init { dir, admin, listen } => init(&dir, &admin, &listen, input, out).await,
        Command::Serve { .. } => Err(CliError::Usage("serve is run by main".into())),
        Command::User { config, action } => {
            let (meta, _) = open(&Config::load(&config)?).await?;
            match action {
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
            let (meta, _) = open(&Config::load(&config)?).await?;
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
        Command::ResetEpoch { config, username } => {
            let (meta, _) = open(&Config::load(&config)?).await?;
            let user = user_id(&meta, &username).await?;
            let epoch = meta.reset_epoch(user.space).await?;
            writeln!(out, "new epoch {epoch} for {}", user.username)?;
            Ok(())
        }
    }
}

async fn init(dir: &Path, admin: &str, listen: &str, input: &mut impl BufRead, out: &mut impl Write) -> Result<(), CliError> {
    if dir.exists() && std::fs::read_dir(dir)?.next().is_some() {
        return Err(CliError::Usage(format!("{} is not empty", dir.display())));
    }
    let listen: std::net::SocketAddr = listen.parse().map_err(|_| CliError::Usage(format!("bad listen address {listen}")))?;
    let password = read_password(input)?;
    std::fs::create_dir_all(dir)?;
    let dir = std::fs::canonicalize(dir)?;
    let db = dir.join("meta.db");
    let blobs = dir.join("blobs");
    let config_text = format!(
        "listen = \"{listen}\"\nbehind_proxy = false\ntrusted_proxies = []\n\n[database]\nurl = {}\n\n[storage]\nkind = \"disk\"\npath = {}\n",
        toml_string(&format!("sqlite:{}", db.display())),
        toml_string(&blobs.display().to_string()),
    );
    let config = Config::parse(&config_text)?;
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, config_text)?;
    let (meta, _) = open(&config).await?;
    let hash = hash_password(&password).map_err(|e| CliError::Usage(e.message))?;
    meta.create_user(admin, &hash, Role::Admin, now_ms()).await?;
    writeln!(out, "created {}\nrun: craft-cloud-server serve --config {}", dir.display(), config_path.display())?;
    Ok(())
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
    fn the_cli_parses() {
        let cli = Cli::try_parse_from(["craft-cloud-server", "user", "--config", "c.toml", "add", "ana", "--admin"]).unwrap();
        assert!(matches!(cli.command, Command::User { action: UserAction::Add { admin: true, .. }, .. }));
    }
}
