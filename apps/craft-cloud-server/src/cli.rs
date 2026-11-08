//! The `craft-cloud-server` command line. Passwords are read from standard input (one line), so they never
//! appear in the process list or shell history: `printf '%s\n' "$PASSWORD" | craft-cloud-server user add …`.

use std::io::{BufRead, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use craft_cloud_meta::{ActionFilter, AuditQuery, Meta, Origin, RevokeReason, Role, SpaceActor, valid_username};
use craft_cloud_proto::api::SpaceRole;
use craft_cloud_proto::join::{Code, INVITE_CHARS, INVITE_TTL_MS, JoinString};
use craft_cloud_proto::{DeviceId, SpaceId, SpaceName};
use craft_cloud_store::Store;
use craft_cloud_store::disk::DiskBackend;
use craft_cloud_store::s3::S3Backend;

use crate::auth::{check_password_rules, hash_password};
use crate::config::{Config, StorageConfig};
use crate::secretbox::TwoFactorKey;
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
    /// Manage team spaces: create one with its first owner, list them, see and add members, archive.
    Space {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: SpaceAction,
    },
    /// Start a new epoch for a user's space, a team space (--space) or every space with --all. Run after restoring the server
    /// from a backup.
    #[command(group(clap::ArgGroup::new("target").required(true).multiple(false).args(["username", "all", "space"])))]
    ResetEpoch {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        /// The user whose space to reset.
        username: Option<String>,
        /// Reset every space on the server.
        #[arg(long)]
        all: bool,
        /// Reset this space, by ID (a team space's, or any user's).
        #[arg(long)]
        space: Option<SpaceId>,
    },
    /// Read and prune the security audit log.
    Audit {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: AuditCommand,
    },
    /// Check the database and the blob store against each other and report: the reference index of every manifest, the
    /// manifests and chunks the database names against the objects in the store, and what names what. Changes nothing;
    /// exits with an error when it finds a problem. Reads every chunk object (`--skip-chunk-reads` leaves that out).
    Verify {
        /// Path to config.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Print the report as JSON.
        #[arg(long)]
        json: bool,
        /// Check chunks against the database only; do not read their objects from the store.
        #[arg(long)]
        skip_chunk_reads: bool,
    },
    /// Inventory the store and report possible collection candidates without changing anything.
    Gc {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Clean up the admin pages' browser sessions.
    WebSessions {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: WebSessionsCommand,
    },
    /// Manage the key for two-factor secrets.
    TwoFactor {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: TwoFactorAction,
    },
    /// The reference index of what each manifest names.
    Index {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: IndexAction,
    },
    /// Manage live documents.
    Live {
        /// Path to config.toml. May come before or after the action.
        #[arg(long, global = true)]
        config: Option<PathBuf>,
        #[command(subcommand)]
        action: LiveAction,
    },
    /// Ask a running server whether it answers (`GET /api/info`). Prints nothing and exits 0 if it does; otherwise
    /// exits 1 with one line on standard error. For a container's health check, which has no shell and no `curl`.
    Healthcheck {
        /// The address the server listens on, as seen from here.
        #[arg(long, default_value = "127.0.0.1:8080")]
        addr: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum SpaceAction {
    /// Create a team space. Its first owner is a member from the start, with no invitation to accept.
    Create {
        /// The space's display name (1 to 64 characters, unique on the server in any case).
        name: String,
        /// The first owner's username.
        #[arg(long)]
        owner: String,
    },
    /// List the team spaces: ID, members, owners, stored bytes and name.
    List,
    /// List a team space's members and invitations. The space is its ID or its name.
    Members { space: String },
    /// Add an account to a team space, accepted at once (whoever runs the command line has the files anyway).
    Add {
        /// The space's ID or name.
        space: String,
        /// The account's username.
        user: String,
        /// viewer, editor or owner.
        #[arg(long, default_value = "editor")]
        role: String,
    },
    /// Archive a team space: every member becomes read-only, nothing is deleted.
    Archive {
        /// The space's ID or name.
        space: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuditCommand {
    /// Print the newest security events, newest first, as text. An event names accounts by ID, never by name.
    List {
        /// Only events this account acted in or was the target of.
        #[arg(long)]
        user: Option<String>,
        /// Only this event (like auth.login) or a group of them (like auth.).
        #[arg(long)]
        action: Option<String>,
        /// How many events at most (1 to 200).
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Delete the events older than `[audit] retention_days` and say how many. Run it now and then (daily, from cron or
    /// the task scheduler): the server also prunes when it writes an event, but not while no one signs in or changes anything.
    Prune {
        /// Say how many events would go and delete none.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum WebSessionsCommand {
    /// Delete the browser sessions that are over (logged out, past `[web_admin] absolute_hours`, or unused for longer than
    /// `idle_minutes`) and say how many. The server also deletes a few at each sign-in, but not while no one signs in; run this
    /// now and then (daily, from cron or the task scheduler) to empty the table. A session that is still valid is never touched.
    Prune {
        /// Say how many sessions would go and delete none.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum TwoFactorAction {
    /// Rewrite every two-factor secret that is sealed with the older key (`key_file_old`) under the current one
    /// (`key_file`). To rotate the key: put the new key in `key_file` and the old one in `key_file_old`, run this, then
    /// remove `key_file_old`. It can be run again, changes nothing when it is done, and refuses (changing nothing) when a
    /// secret is sealed with a key that is neither. Prints counts, never a secret or a key. Restart every server instance on
    /// the database with the new `key_file` and `key_file_old` first (a running server holds the keys it started with), and
    /// remove `key_file_old` only after this reports that every secret is under the current key.
    Rekey,
}

#[derive(Debug, Subcommand)]
pub enum IndexAction {
    /// Complete the index of every manifest whose index is not complete: reads each manifest from the store, checks it
    /// against its ID, and writes its edges in batches. Writes only that derived index. Safe to stop and run again, and to
    /// run beside the server.
    Build {
        /// Edges written per database transaction (default 5000).
        #[arg(long)]
        batch_rows: Option<usize>,
    },
}

#[derive(Debug, Subcommand)]
pub enum LiveAction {
    /// Empty a user's recents document of an app (one that is full: `quota_exceeded`), or, given a document's ID
    /// instead of an app, that live document of a file. Its log and its snapshot go; every machine then sends its whole
    /// document up again, so nothing any machine holds is lost. With `--space` (a team space's ID or name) the document
    /// is the team space's: `username` must be one of its members.
    ResetDoc {
        username: String,
        app: String,
        #[arg(long)]
        space: Option<String>,
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
    /// Set a new password, read from standard input. Every device of the account is signed out and its open invite
    /// and add-a-computer codes are deleted, so a stolen device or code does not outlive the old password; with
    /// `--keep-devices` the devices stay signed in. A device's live connection ends at its next ping (about 30
    /// seconds) and a change-feed long-poll within 60 seconds, since the server runs in another process.
    ResetPassword {
        username: String,
        /// Leave the account's devices signed in.
        #[arg(long)]
        keep_devices: bool,
    },
    /// Take away an account's two-factor sign-in, for someone who lost their authenticator and their recovery codes: its
    /// secret, recovery codes and open sign-in challenges are deleted, the lock after wrong codes goes, **every device of the
    /// account is signed out** and its open invite and add-a-computer codes are deleted. Prints how many devices were signed
    /// out. The person signs in with their password again and can set it up anew. A device's live connection ends at its next
    /// ping (about 30 seconds), since the server runs in another process.
    #[command(name = "reset-2fa")]
    Reset2fa { username: String },
    /// Print an invite code for an account, good once for 7 days, to join a computer with in Craft Cloud. Creates the
    /// account (without a password, chosen when the invite is redeemed) if there is none, and replaces its earlier
    /// invite or password reset code.
    Invite {
        username: String,
        /// Revoke the account's invite instead.
        #[arg(long)]
        revoke: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum DeviceAction {
    List {
        username: String,
    },
    Revoke {
        username: String,
        device: DeviceId,
    },
    /// Sign out every device of an account. Live connections end at their next ping (about 30 seconds), change-feed
    /// long-polls within 60 seconds.
    RevokeAll {
        username: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigError),
    #[error(transparent)]
    Meta(#[from] craft_cloud_meta::MetaError),
    #[error(transparent)]
    Store(#[from] craft_cloud_store::StoreError),
    #[error(transparent)]
    Gc(#[from] crate::gc::GcError),
    #[error("{0}")]
    Usage(String),
    /// A check found something wrong (its report was written first).
    #[error("{0}")]
    Inconsistent(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Opens the database and blob storage a config names.
pub async fn open(config: &Config) -> Result<(Meta, Store), CliError> {
    let meta = Meta::open(&config.database.url)
        .await?
        .with_stored_bytes_cap(config.stored_bytes_per_user)
        .with_live_bytes_cap(config.live_bytes_per_user)
        .with_team_live_bytes_cap(config.spaces.team_live_bytes)
        .with_audit_retention_days(config.audit.retention_days);
    let store = match (&config.storage, config.storage.s3()?) {
        (_, Some(s3)) => Store::new(S3Backend::new(&s3)?),
        (StorageConfig::Disk { path }, None) => Store::new(DiskBackend::new(path)?),
        (StorageConfig::S3 { .. }, None) => return Err(CliError::Usage("S3 storage without settings".into())),
    };
    Ok((meta, store))
}

/// Like [`open`], for a command that only reads: the database and the storage must exist already, and nothing is
/// created or upgraded. A config that points at the wrong place is an error, not a new empty server that has no problems.
pub async fn open_existing(config: &Config) -> Result<(Meta, Store), CliError> {
    let meta = Meta::open_existing(&config.database.url).await?;
    let store = match (&config.storage, config.storage.s3()?) {
        (_, Some(s3)) => Store::new(S3Backend::new(&s3)?),
        (StorageConfig::Disk { path }, None) => {
            if !path.is_dir() {
                return Err(CliError::Usage(format!("the storage directory {} does not exist (is the config's path right?)", path.display())));
            }
            Store::new(DiskBackend::new(path)?)
        }
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

/// A space named on the command line: its ID, or the name of a team space.
async fn resolve_space(meta: &Meta, text: &str) -> Result<SpaceId, CliError> {
    if let Ok(id) = text.parse::<SpaceId>() {
        return Ok(id);
    }
    let name = SpaceName::new(text).map_err(|e| CliError::Usage(format!("{text:?} is neither a space ID nor a space name: {e}")))?;
    meta.team_space_by_name(&name).await?.ok_or_else(|| CliError::Usage(format!("no team space {text:?}")))
}

async fn space_command(meta: &Meta, action: SpaceAction, out: &mut impl Write) -> Result<(), CliError> {
    match action {
        SpaceAction::Create { name, owner } => {
            let name = SpaceName::new(&name).map_err(|e| CliError::Usage(format!("a space's name: {e}")))?;
            let owner = user_id(meta, &owner).await?;
            let space = meta.create_team_space(&SpaceActor::Cli, &name, owner.id, None, now_ms()).await?;
            writeln!(out, "created the team space {space}, owned by {}", owner.username)?;
        }
        SpaceAction::List => {
            let mut after = None;
            loop {
                let page = meta.admin_list_spaces(after.as_deref(), None, 200).await?;
                for space in &page.spaces {
                    let archived = if space.archived_ms.is_some() { "  (archived)" } else { "" };
                    writeln!(
                        out,
                        "{}  {} member(s), {} owner(s), {} bytes  {}{archived}",
                        space.id, space.members, space.owners, space.stored_bytes, space.name
                    )?;
                }
                match page.next_after {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
        }
        SpaceAction::Members { space } => {
            let space = resolve_space(meta, &space).await?;
            for member in meta.list_members(space).await? {
                writeln!(out, "{}  {}  {}", member.username, member.role.as_str(), if member.accepted() { "member" } else { "invited" })?;
            }
        }
        SpaceAction::Add { space, user, role } => {
            let space = resolve_space(meta, &space).await?;
            let role = SpaceRole::parse(&role).ok_or_else(|| CliError::Usage("the role is viewer, editor or owner".into()))?;
            let user = user_id(meta, &user).await?;
            let current = meta.list_members(space).await?.into_iter().find(|m| m.user == user.id);
            match current {
                None => {
                    meta.invite_member(&SpaceActor::Cli, space, user.id, role, now_ms()).await?;
                }
                // Already in the space: the command sets the role instead, and accepts a pending invitation (whoever runs the
                // command line has the files).
                Some(member) => {
                    meta.change_member_role(&SpaceActor::Cli, space, user.id, role, now_ms()).await?;
                    if !member.accepted() {
                        writeln!(out, "{} had only been invited so far; the invitation is accepted now", user.username)?;
                    }
                }
            }
            writeln!(out, "{} is {} of the space", user.username, role.as_str())?;
        }
        SpaceAction::Archive { space } => {
            let space = resolve_space(meta, &space).await?;
            meta.set_space_archived(&SpaceActor::Cli, space, true, now_ms()).await?;
            writeln!(out, "archived the space {space}; nothing was deleted")?;
        }
    }
    Ok(())
}

/// Runs every command except `serve` (which `main` runs, since it never returns).
pub async fn run(command: Command, input: &mut impl BufRead, out: &mut impl Write) -> Result<(), CliError> {
    match command {
        Command::Init { dir, admin, listen, behind_proxy, trusted_proxies } => {
            init(&dir, &admin, &listen, (behind_proxy, &trusted_proxies), input, out).await
        }
        Command::Serve { .. } => Err(CliError::Usage("serve is run by main".into())),
        Command::Healthcheck { addr } => {
            let addr: SocketAddr = addr.parse().map_err(|_| CliError::Usage(format!("bad address {addr}")))?;
            // Blocking for at most `HEALTH_TIMEOUT`: this process does nothing else.
            check_health(addr, HEALTH_TIMEOUT)
        }
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
                UserAction::Reset2fa { username } => {
                    let user = user_id(&meta, &username).await?;
                    let revoked = meta.reset_two_factor(user.id, now_ms()).await?;
                    tracing::info!(revoked = revoked.len(), "two-factor sign-in reset");
                    writeln!(out, "two-factor sign-in of {} is off; signed out {} device(s)", user.username, revoked.len())?;
                }
                UserAction::Add { username, admin } => {
                    let password = read_password(input)?;
                    let role = if admin { Role::Admin } else { Role::Member };
                    let hash = hash_password(&password).map_err(|e| CliError::Usage(e.message))?;
                    let user = meta.create_user(&username, &hash, role, now_ms()).await?;
                    writeln!(out, "added {} ({})", user.username, if admin { "admin" } else { "member" })?;
                }
                UserAction::ResetPassword { username, keep_devices } => {
                    let user = user_id(&meta, &username).await?;
                    let password = read_password(input)?;
                    let hash = hash_password(&password).map_err(|e| CliError::Usage(e.message))?;
                    let revoked = meta.reset_password(user.id, &hash, keep_devices, now_ms()).await?;
                    tracing::info!(revoked = revoked.len(), "password reset");
                    if keep_devices {
                        writeln!(out, "password changed for {}; the devices stay signed in", user.username)?;
                    } else {
                        writeln!(out, "password changed for {}; signed out {} device(s)", user.username, revoked.len())?;
                    }
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
                    meta.revoke_device_by(user.id, device, RevokeReason::Admin, Origin::Cli, now_ms()).await?;
                    writeln!(out, "revoked {device}")?;
                }
                DeviceAction::RevokeAll { username } => {
                    let user = user_id(&meta, &username).await?;
                    let revoked = meta.revoke_all_devices(user.id, RevokeReason::Admin, now_ms()).await?;
                    tracing::info!(revoked = revoked.len(), "all devices revoked");
                    writeln!(out, "revoked {} device(s) of {}", revoked.len(), user.username)?;
                }
            }
            Ok(())
        }
        Command::Audit { config, action } => {
            let config = Config::load(&config_path(config)?)?;
            let (meta, _) = open(&config).await?;
            audit(&meta, &config, action, out).await
        }
        Command::WebSessions { config, action } => {
            let config = Config::load(&config_path(config)?)?;
            let (meta, _) = open(&config).await?;
            web_sessions(&meta, &config, action, out).await
        }
        Command::Space { config, action } => {
            let (meta, _) = open(&Config::load(&config_path(config)?)?).await?;
            space_command(&meta, action, out).await
        }
        Command::ResetEpoch { config, username, all, space } => {
            let (meta, _) = open(&Config::load(&config_path(config)?)?).await?;
            if let Some(space) = space {
                let epoch = meta.reset_epoch(space).await?;
                writeln!(out, "new epoch {epoch} for space {space}")?;
            } else if all {
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
        Command::TwoFactor { config, action: TwoFactorAction::Rekey } => {
            let config = Config::load(&config_path(config)?)?;
            let (meta, _) = open(&config).await?;
            let keys = crate::two_factor::load_keys(&config).map_err(|e| CliError::Usage(e.to_string()))?;
            let crate::two_factor::KeyState::Loaded(secret_box) = keys else {
                return Err(CliError::Usage(
                    "there is no key: set [two_factor] key_file (and key_file_old for the key to rotate away from) in the config".into(),
                ));
            };
            let done = crate::two_factor::rekey(&meta, &secret_box).await.map_err(CliError::Usage)?;
            writeln!(out, "rewrote {} secret(s) under the current key; {} were under it already", done.rewritten, done.already_current)?;
            if secret_box.old_id().is_some() {
                writeln!(
                    out,
                    "every secret is under the current key now. key_file_old can be removed once every server instance on this database has been \
                     restarted with key_file (the new key) and key_file_old, as it must have been before this command: an instance still running with \
                     only the old key would seal new secrets with it"
                )?;
            }
            Ok(())
        }
        Command::Verify { config, json, skip_chunk_reads } => {
            let (meta, store) = open_existing(&Config::load(&config_path(config)?)?).await?;
            let report = crate::verify::verify(&meta, &store, crate::verify::VerifyOptions { read_chunks: !skip_chunk_reads }).await?;
            if json {
                writeln!(out, "{}", report.to_json())?;
            } else {
                write!(out, "{}", report.to_text())?;
            }
            match report.problem_count() {
                0 => Ok(()),
                n => Err(CliError::Inconsistent(format!("verify found {n} problem(s)"))),
            }
        }
        Command::Gc { dry_run: false, .. } => Err(CliError::Usage("deleting is not implemented yet; run with --dry-run".into())),
        Command::Gc { config, dry_run: true, json } => {
            let config = Config::load(&config_path(config)?)?;
            let (meta, store) = open_existing(&config).await?;
            let report = crate::gc::dry_run(&meta, &store).await?;
            if json {
                writeln!(out, "{}", report.to_json())?;
            } else {
                write!(out, "{}", report.to_text())?;
            }
            if report.has_problems() { Err(CliError::Inconsistent("gc dry-run found problems".into())) } else { Ok(()) }
        }
        Command::Index { config, action: IndexAction::Build { batch_rows } } => {
            let (meta, store) = open(&Config::load(&config_path(config)?)?).await?;
            let meta = match batch_rows {
                Some(0) => return Err(CliError::Usage("--batch-rows must be at least 1".into())),
                Some(rows) => meta.with_index_batch_rows(rows),
                None => meta,
            };
            let report = crate::verify::build_index(&meta, &store).await?;
            writeln!(out, "indexed {} manifest(s)", report.indexed)?;
            for (id, why) in &report.unreadable {
                writeln!(out, "could not index manifest {id}: {why}")?;
            }
            if report.unreadable.is_empty() {
                Ok(())
            } else {
                Err(CliError::Inconsistent(format!("{} manifest(s) could not be indexed", report.unreadable.len())))
            }
        }
        Command::Live { config, action: LiveAction::ResetDoc { username, app, space } } => {
            let (meta, _) = open(&Config::load(&config_path(config)?)?).await?;
            let user = user_id(&meta, &username).await?;
            // The user's own space, or the team space `--space` names (which the user must be able to read).
            let (space, whose) = match space {
                None => (user.space, user.username.clone()),
                Some(text) => {
                    let space = resolve_space(&meta, &text).await?;
                    if meta.space_access(user.id, space).await?.is_none() {
                        return Err(CliError::Usage(format!("{} is not a member of that space", user.username)));
                    }
                    (space, format!("the space {text}"))
                }
            };
            if let Ok(id) = app.parse::<uuid::Uuid>() {
                let doc = craft_cloud_proto::DocId::from_uuid(id);
                meta.reset_live_doc(space, doc).await?;
                writeln!(out, "emptied the live document {doc} of {whose}; every machine sends its own again when it next connects")?;
                return Ok(());
            }
            let app = craft_cloud_proto::live::AppName::new(&app).map_err(|e| CliError::Usage(e.to_string()))?;
            let doc = craft_cloud_proto::live::recents_doc(space, &app);
            meta.reset_live_doc(space, doc).await?;
            writeln!(out, "emptied the {app} recents of {whose}; every machine sends its own again when it next connects")?;
            Ok(())
        }
    }
}

/// How long `healthcheck` waits for the whole exchange: connecting, sending and reading the status line.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

/// Sends `GET /api/info` to `addr` and succeeds only if the status line says 200. The whole exchange must finish
/// within `timeout`. The body is never read, and nothing the server sends is printed.
pub fn check_health(addr: SocketAddr, timeout: Duration) -> Result<(), CliError> {
    let fail = |what: &str| CliError::Usage(format!("{addr} is not healthy: {what}"));
    let deadline = Instant::now() + timeout;
    let left = || deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero());
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|e| fail(&format!("can't connect: {e}")))?;
    let request = format!("GET /api/info HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream.set_write_timeout(left()).map_err(|e| fail(&e.to_string()))?;
    stream.write_all(request.as_bytes()).map_err(|e| fail(&format!("can't send the request: {e}")))?;
    // The status line fits in 64 bytes. It counts only when it is complete (ends in CRLF): an answer that stops inside
    // it, or goes on for 64 bytes without a line end, is not a healthy server.
    let mut head = [0u8; 64];
    let mut len = 0;
    let line_end = loop {
        if let Some(end) = head.get(..len).and_then(|read| read.windows(2).position(|pair| pair == b"\r\n")) {
            break end;
        }
        let wait = left().ok_or_else(|| fail("no answer in time"))?;
        stream.set_read_timeout(Some(wait)).map_err(|e| fail(&e.to_string()))?;
        let Some(room) = head.get_mut(len..).filter(|room| !room.is_empty()) else { return Err(fail("not an HTTP answer")) };
        match stream.read(room) {
            Ok(0) => return Err(fail("the answer ended inside the status line")),
            Ok(n) => len += n,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Err(fail("no answer in time")),
            Err(e) => return Err(fail(&format!("can't read the answer: {e}"))),
        }
    };
    let line = String::from_utf8_lossy(head.get(..line_end).unwrap_or_default());
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next()) {
        (Some(version), Some("200")) if version.starts_with("HTTP/1.") => Ok(()),
        (Some(version), Some(status)) if version.starts_with("HTTP/") => Err(fail(&format!("status {}", status.chars().take(3).collect::<String>()))),
        _ => Err(fail("not an HTTP answer")),
    }
}

/// Sessions one batch of `web-sessions prune` deletes.
const WEB_SESSION_PRUNE_BATCH: u32 = 1000;

/// `web-sessions prune`: the batches go on until none is left, each in a transaction of its own.
async fn web_sessions(meta: &Meta, config: &Config, action: WebSessionsCommand, out: &mut impl Write) -> Result<(), CliError> {
    let idle_ms = config.web_admin.idle_ms();
    let WebSessionsCommand::Prune { dry_run } = action;
    if dry_run {
        let due = meta.web_sessions_count_over(now_ms(), idle_ms).await?;
        writeln!(out, "{due} web session(s) are over; dry run, nothing deleted")?;
        return Ok(());
    }
    let mut deleted = 0u64;
    loop {
        let batch = meta.web_sessions_prune(now_ms(), idle_ms, WEB_SESSION_PRUNE_BATCH).await?;
        deleted = deleted.saturating_add(batch);
        if batch < u64::from(WEB_SESSION_PRUNE_BATCH) {
            break;
        }
    }
    tracing::info!(deleted, "web sessions pruned");
    writeln!(out, "deleted {deleted} web session(s) that were over")?;
    Ok(())
}

/// `audit list` and `audit prune`.
async fn audit(meta: &Meta, config: &Config, action: AuditCommand, out: &mut impl Write) -> Result<(), CliError> {
    match action {
        AuditCommand::List { user, action, limit } => {
            if !(1..=200).contains(&limit) {
                return Err(CliError::Usage("--limit is 1 to 200".into()));
            }
            let user = match user {
                Some(name) => Some(user_id(meta, &name).await?.id),
                None => None,
            };
            let action = match action {
                Some(text) => Some(
                    ActionFilter::parse(&text)
                        .ok_or_else(|| CliError::Usage("--action is an event name like auth.login, or a group like auth.".into()))?,
                ),
                None => None,
            };
            let events = meta.audit_list(&AuditQuery { user, action, limit, ..AuditQuery::default() }).await?;
            let id = |id: Option<uuid::Uuid>| id.map_or_else(|| "-".to_owned(), |id| id.to_string());
            for e in &events {
                // Only closed words, IDs and numbers: nothing here is a secret or a name anyone typed.
                writeln!(
                    out,
                    "{}  {}  {}  actor={}  device={}  target={}:{}  count={}  {}",
                    e.at_ms,
                    e.action.as_str(),
                    e.outcome.as_str(),
                    id(e.actor_user.map(|u| u.0)),
                    id(e.actor_device.map(|d| *d.as_uuid())),
                    if e.target_kind.is_empty() { "-" } else { e.target_kind.as_str() },
                    id(e.target_id),
                    e.count,
                    e.details,
                )?;
            }
            writeln!(out, "{} event(s)", events.len())?;
        }
        AuditCommand::Prune { dry_run } => {
            let days = config.audit.retention_days;
            let before = now_ms().saturating_sub(i64::from(days).saturating_mul(24 * 60 * 60 * 1000));
            if dry_run {
                let due = meta.audit_count_before(before).await?;
                writeln!(out, "{due} audit event(s) are older than {days} days; dry run, nothing deleted")?;
            } else {
                let deleted = meta.audit_prune(before).await?;
                tracing::info!(deleted, "audit events pruned");
                writeln!(out, "deleted {deleted} audit event(s) older than {days} days")?;
            }
        }
    }
    Ok(())
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
        "listen = \"{listen}\"\nbehind_proxy = {behind_proxy}\ntrusted_proxies = [{}]\n\n[database]\nurl = {}\n\n[storage]\nkind = \"disk\"\npath = {}\n\n\
         [two_factor]\nkey_file = \"{KEY_FILE_NAME}\"\n",
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
    writeln!(
        out,
        "two-factor key: {}. Keep a copy apart from your database backups: without it the two-factor secrets in a backup can't be opened.",
        dir.join(KEY_FILE_NAME).display()
    )?;
    Ok(())
}

async fn create_files(dir: &Path, config_text: &str, config: &Config, admin: &str, hash: &str) -> Result<(), CliError> {
    create_data_dir(dir)?;
    write_config(&dir.join("config.toml"), config_text)?;
    write_key_file(&dir.join(KEY_FILE_NAME))?;
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

/// The key file `init` makes, next to `config.toml`.
const KEY_FILE_NAME: &str = "two-factor.key";

/// Writes a new random key for two-factor secrets: 32 bytes, owner-only on Unix, and never over a file that is there.
fn write_key_file(path: &Path) -> Result<(), CliError> {
    let key = TwoFactorKey::generate().map_err(|e| CliError::Usage(format!("the system random source failed: {e}")))?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(key.to_file_bytes().as_slice())?;
    Ok(())
}

/// Removes what a failed `init` made. The directory was empty (or absent) when `init` started, so
/// everything in it is `init`'s. Best effort: the original error is what the admin needs to see.
fn remove_made(dir: &Path, created_dir: bool) {
    for name in ["config.toml", KEY_FILE_NAME, "meta.db", "meta.db-wal", "meta.db-shm"] {
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
        for name in ["config.toml", "two-factor.key", "meta.db", "meta.db-wal", "blobs/chunk"] {
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
    fn healthcheck_parses_with_and_without_an_address() {
        let cli = Cli::try_parse_from(["craft-cloud-server", "healthcheck"]).unwrap();
        assert!(matches!(&cli.command, Command::Healthcheck { addr } if addr == "127.0.0.1:8080"));
        let cli = Cli::try_parse_from(["craft-cloud-server", "healthcheck", "--addr", "127.0.0.1:9000"]).unwrap();
        assert!(matches!(&cli.command, Command::Healthcheck { addr } if addr == "127.0.0.1:9000"));
    }

    #[test]
    fn reset_epoch_takes_a_username_or_all_but_not_both_or_neither() {
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml", "ana"]).is_ok());
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml"]).is_err());
        assert!(Cli::try_parse_from(["craft-cloud-server", "reset-epoch", "--config", "c.toml", "ana", "--all"]).is_err());
    }
}
