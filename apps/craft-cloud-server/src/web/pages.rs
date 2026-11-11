//! The pages behind the sign-in: overview, users (list, search, create, one user's page with its changes and devices), and the
//! audit log with its export. Each change is a form that posts to a route of its own, is read and checked, and handed to the
//! same `craft_cloud_meta` call the JSON admin route makes, with the same confirmation (password, and a code when the admin's
//! account has two-factor sign-in). A change that worked redirects (`303`) back to a page that says so; a code the admin made
//! is the answer to the `POST` itself, shown once.
//!
//! The pages decide nothing about who may do what: the role is read from the database for each request
//! ([`super::authenticate`]) and the rules are the store's.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::Response;
#[cfg(test)]
use craft_cloud_meta::StorageOverview;
use craft_cloud_meta::{
    AdminActor, AdminUser, AdminVia, AuditEvent, AuditQuery, DeviceRecord, RESET_TTL_MS, StorageEstimate, UserId, valid_username,
};
use craft_cloud_proto::DeviceId;
use craft_cloud_proto::api::AdminCode;
use craft_cloud_proto::api::{AdminCompression, AdminServerInfo, AdminStorageOverview};
use craft_cloud_proto::join::INVITE_TTL_MS;
use serde::Deserialize;
use uuid::Uuid;

use super::html::{Ctx, checkbox, confirm_fields, field, fmt_bytes, fmt_time, heading, link, notice, page, post_form, select, table, url_encode};
use super::{Authed, Shared, WebError, authenticate, html_response, read_form, read_query, redirect};
use crate::admin::{Confirmation, DEFAULT_PAGE, MAX_QUERY_BYTES, confirm_actor, decode_cursor, end_live_connections, new_code, role_of};
use crate::audit::{Params, encode_cursor, export_response, filters};
use crate::state::AppState;

/// Events one page of the audit log holds.
const AUDIT_PAGE: u32 = 50;
pub(super) const MIB: u64 = 1024 * 1024;

/// Every field any form of the pages has; a form sends the few it needs and the rest stay empty.
#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct ActionForm {
    pub(super) csrf: String,
    pub(super) password: String,
    pub(super) totp_code: String,
    pub(super) recovery_code: String,
    pub(super) username: String,
    pub(super) role: String,
    pub(super) quota_mib: String,
    pub(super) remove_quota: String,
    pub(super) reset_two_factor: String,
    /// A team space's name (the Team spaces page).
    pub(super) name: String,
    /// The account name of a team space's owner (the Team spaces page).
    pub(super) owner: String,
    pub(super) compression_level: String,
}

impl std::fmt::Debug for ActionForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActionForm").field("fields", &"<redacted>").finish()
    }
}

#[derive(Deserialize, Default)]
struct UsersQuery {
    query: Option<String>,
    cursor: Option<String>,
    notice: Option<String>,
}

#[derive(Deserialize, Default)]
struct NoticeQuery {
    notice: Option<String>,
}

/// What a `?notice=` says. Only these words are ever shown, never the parameter's own text.
fn notice_text(key: Option<&str>) -> Option<&'static str> {
    match key? {
        "updated" => Some("The account was updated."),
        "disabled" => Some("The account is disabled and its devices are signed out."),
        "enabled" => Some("The account is enabled. Its devices stay signed out."),
        "removed" => Some("The account is marked for removal and its devices are signed out. Nothing it stored was deleted."),
        "invite_revoked" => Some("The open invite or reset code was ended."),
        "two_factor_reset" => Some("Two-factor sign-in of the account is off and its devices are signed out."),
        "device_revoked" => Some("The device is signed out."),
        _ => None,
    }
}

fn notice_html(key: Option<&str>) -> String {
    notice_text(key).map(|text| notice(text, false)).unwrap_or_default()
}

pub(super) fn ok_page(ctx: &Ctx, title: &str, body: &str) -> Response {
    html_response(StatusCode::OK, &page(Some(ctx), title, body))
}

fn user_href(id: UserId) -> String {
    format!("/web/users/{}", id.0)
}

/// Whether the admin's own account has two-factor sign-in, which decides whether the forms ask for a code.
pub(super) async fn asks_for_code(state: &AppState, authed: &Authed) -> Result<bool, WebError> {
    Ok(state.meta.two_factor_enabled(authed.session.user).await?)
}

fn parse_user(id: &str) -> Result<UserId, WebError> {
    Uuid::parse_str(id).map(UserId).map_err(|_| WebError::not_found("account"))
}

fn usage_rows(usage: &StorageEstimate) -> Vec<(&'static str, String)> {
    let estimates = usage.reason.is_none();
    vec![
        ("Usage stored", fmt_bytes(usage.stored_bytes)),
        ("Physical", "not measured".to_owned()),
        ("Files", "not measured".to_owned()),
        ("Effective quota", usage.quota_bytes.map_or_else(|| "none".to_owned(), fmt_bytes)),
        (
            "Trash estimate (overlapping)",
            if estimates { usage.trash_bytes.map_or_else(|| "none".to_owned(), fmt_bytes) } else { "estimates not available yet".to_owned() },
        ),
        (
            "History estimate (overlapping)",
            if estimates { usage.history_bytes.map_or_else(|| "none".to_owned(), fmt_bytes) } else { "estimates not available yet".to_owned() },
        ),
    ]
}

#[cfg(test)]
fn storage_section(storage: &StorageOverview) -> String {
    let mut body = heading("Storage");
    body.push_str("<dl>");
    let mut row = |term: &str, value: String| {
        body.push_str(&format!("<dt>{}</dt><dd>{}</dd>", super::html::esc(term), super::html::esc(&value)));
    };
    row("Accounts", storage.accounts.to_string());
    row("Stored", fmt_bytes(storage.stored_bytes));
    row("Default quota", storage.default_quota_bytes.map_or_else(|| "none".to_owned(), fmt_bytes));
    row("Accounts with quota override", storage.accounts_with_override.to_string());
    row("Accounts near quota", storage.accounts_near_quota.to_string());
    row("Accounts over quota", storage.accounts_over_quota.to_string());
    row(
        "Manifest index",
        format!(
            "{} complete, {} building, {} not indexed",
            storage.manifest_index.complete, storage.manifest_index.building, storage.manifest_index.not_indexed
        ),
    );
    body.push_str("</dl>");
    if storage.manifest_index.building > 0 || storage.manifest_index.not_indexed > 0 {
        body.push_str(&notice("Storage estimates are incomplete; run `craft-cloud-server index build`.", false));
    }
    let rows: Vec<Vec<String>> = storage
        .top_accounts
        .iter()
        .map(|account| {
            vec![
                link(&user_href(account.id), &account.username),
                super::html::esc(&fmt_bytes(account.stored_bytes)),
                super::html::esc(&account.quota_bytes.map_or_else(|| "none".to_owned(), fmt_bytes)),
            ]
        })
        .collect();
    if rows.is_empty() {
        body.push_str("<p>No accounts.</p>");
    } else {
        body.push_str(&table("Top accounts by stored bytes", &["Username", "Stored", "Effective quota"], &rows));
    }
    body
}

fn server_health(info: &AdminServerInfo) -> String {
    let mut rows = Vec::new();
    if info.database_ok == Some(false) {
        rows.push("Database not answering");
    }
    if info.store_ok == Some(false) {
        rows.push("Store not answering");
    }
    if info.refusing_uploads == Some(true) {
        rows.push("Refusing uploads");
    }
    if rows.is_empty() {
        return "<h2>Server health</h2><p class=\"muted\">Healthy</p>".to_owned();
    }
    format!("<h2>Server health</h2>{}", notice(&rows.join(" · "), true))
}

fn compression_section(compression: &AdminCompression) -> String {
    let mut body = String::from("<h2>Compression</h2><dl>");
    let mut row = |term: &str, value: String| {
        body.push_str(&format!("<dt>{}</dt><dd>{}</dd>", super::html::esc(term), super::html::esc(&value)));
    };
    row("Algorithm", compression.algorithm.clone());
    row("Level in use", format!("{} · {}", compression.level, compression.admin_level.map_or("from the config", |_| "set by an admin")));
    row("Logical size, all blobs", format!("{} · {} blobs", fmt_bytes(compression.logical_bytes), compression.blobs));
    row(
        "Stored size of the known part",
        format!("{} of {}", fmt_bytes(compression.known_physical_bytes), fmt_bytes(compression.known_logical_bytes)),
    );
    row("Saving", compression.saving_permille.map_or_else(|| "not measured".to_owned(), |saving| format!("{}.{:01} %", saving / 10, saving % 10)));
    row("Known share of the data", format!("{}.{:01} %", compression.known_permille / 10, compression.known_permille % 10));
    if let Some(estimate) = &compression.estimate {
        row("Last estimate", fmt_time(estimate.at_ms));
        row("Estimate sample", format!("{} blobs, {}", estimate.sampled_blobs, fmt_bytes(estimate.sampled_bytes)));
        let levels = estimate
            .levels
            .iter()
            .map(|level| {
                format!("{}: {}.{:01} % · {} KiB/s", level.level, level.saving_permille / 10, level.saving_permille % 10, level.kib_per_second)
            })
            .collect::<Vec<_>>()
            .join("; ");
        row("Per-level estimates", levels);
    } else {
        row("Per-level estimates", "not measured".to_owned());
    }
    body.push_str("</dl>");
    body
}

/// `GET /web/storage`: server and account storage numbers, remembered estimates, and the confirmed compression form.
pub(super) async fn storage(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let authed = authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let q: NoticeQuery = read_query(&request)?;
    let overview = crate::admin::storage_overview(&state).await?;
    let compression = overview.compression.as_deref().ok_or_else(|| WebError::invalid("Storage information is unavailable."))?;
    let ctx = authed.ctx();
    let two_factor = asks_for_code(&state, &authed).await?;
    let mut body = heading("Storage");
    body.push_str(&storage_notice(q.notice.as_deref()));
    body.push_str(&storage_details(&overview));
    body.push_str(&compression_section(compression));
    let labels = (craft_cloud_store::MIN_ZSTD_LEVEL..=craft_cloud_store::MAX_ZSTD_LEVEL).map(|level| level.to_string()).collect::<Vec<_>>();
    let options = labels.iter().map(|level| (level.as_str(), level.as_str())).collect::<Vec<_>>();
    let current = compression.level.to_string();
    if compression.algorithm == "none" {
        body.push_str(
            "<p class=\"muted\">Set by the server config: compression = \"none\" stores blobs plain, so there is no level to change here.</p>",
        );
    } else {
        let mut inner = select("compression", "compression_level", "Level for new files (1 to 19)", &options, &current);
        inner.push_str(
            "<p class=\"muted\">A change needs your password. It applies to files stored from now on; files already stored are not rewritten.</p>",
        );
        inner.push_str(&confirm_fields("compression", two_factor));
        inner.push_str("<p><button type=\"submit\">Change level</button></p>");
        body.push_str(&post_form(&ctx, "/web/storage", "panel", &inner));
    }
    Ok(ok_page(&ctx, "Storage", &body))
}

fn storage_notice(key: Option<&str>) -> String {
    match key {
        Some("compression") => notice("The compression level was updated.", false),
        _ => String::new(),
    }
}

/// `POST /web/storage`: the level is audited through the same metadata operation as the JSON admin route.
pub(super) async fn set_compression(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let level = form
        .compression_level
        .trim()
        .parse::<u8>()
        .ok()
        .filter(|level| (1..=19).contains(level))
        .ok_or_else(|| WebError::invalid("The compression level is 1 to 19."))?;
    let actor = actor(&state, &authed, &form).await?;
    crate::admin::set_compression_level(&state, &actor, level).await?;
    tracing::info!(level, "admin (web): compression level changed");
    Ok(redirect("/web/storage?notice=compression"))
}

fn storage_details(storage: &AdminStorageOverview) -> String {
    let capacity = storage.capacity_bytes.filter(|bytes| *bytes > 0);
    let mut body = String::from("<h2>Server storage</h2><div class=\"stats\" aria-label=\"Storage totals\">");
    body.push_str(&super::html::stat(&fmt_bytes(storage.stored_bytes), "Stored"));
    body.push_str(&super::html::stat(&capacity.map_or_else(|| "—".to_owned(), fmt_bytes), "Capacity"));
    body.push_str(&super::html::stat(&storage.database_bytes.map_or_else(|| "—".to_owned(), fmt_bytes), "Database size"));
    body.push_str("</div><dl>");
    let mut row = |term: &str, value: String| {
        body.push_str(&format!("<dt>{}</dt><dd>{}</dd>", super::html::esc(term), super::html::esc(&value)));
    };
    row("Store", storage.store_kind.clone().unwrap_or_else(|| "not reported".to_owned()));
    row("Capacity", capacity.map_or_else(|| "not configured".to_owned(), fmt_bytes));
    row(
        "Capacity used",
        capacity.map_or_else(
            || "not configured".to_owned(),
            |total| {
                let percent = u128::from(storage.stored_bytes).saturating_mul(100) / u128::from(total);
                format!("{} of {} ({}%)", fmt_bytes(storage.stored_bytes), fmt_bytes(total), percent)
            },
        ),
    );
    let fs = storage.store_fs_total_bytes.zip(storage.store_fs_free_bytes);
    row(
        "Store filesystem",
        fs.map_or_else(
            || "not measured".to_owned(),
            |(total, free)| format!("{} used of {}; {} available", fmt_bytes(total.saturating_sub(free)), fmt_bytes(total), fmt_bytes(free)),
        ),
    );
    let data = storage.server_fs_total_bytes.zip(storage.server_fs_free_bytes);
    row(
        "Data directory filesystem",
        data.map_or_else(
            || "not measured".to_owned(),
            |(total, free)| format!("{} used of {}; {} available", fmt_bytes(total.saturating_sub(free)), fmt_bytes(total), fmt_bytes(free)),
        ),
    );
    row(
        "Manifest index",
        format!(
            "{} complete, {} building, {} not indexed",
            storage.manifest_index.complete, storage.manifest_index.building, storage.manifest_index.not_indexed
        ),
    );
    body.push_str("</dl>");
    let rows = storage
        .top_accounts
        .iter()
        .map(|account| {
            vec![
                link(&user_href(UserId(account.id)), &account.username),
                super::html::esc(&fmt_bytes(account.stored_bytes)),
                super::html::esc(&account.quota_bytes.map_or_else(|| "none".to_owned(), fmt_bytes)),
            ]
        })
        .collect::<Vec<_>>();
    if rows.is_empty() {
        body.push_str("<p>No accounts.</p>");
    } else {
        body.push_str(&table("Top accounts by stored bytes", &["Account", "Stored", "Effective quota"], &rows));
    }
    body
}

// ---- overview ----

/// `GET /web`.
pub(super) async fn overview(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let authed = authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let counts = state.meta.admin_server_counts().await?;
    let server = crate::admin::server_info(&state).await?;
    let storage = state.meta.admin_storage_overview().await?;
    let mut body = heading("Overview");
    if let Some(update) = state.update_info().await.filter(|update| update.available) {
        body.push_str(&update_banner(&state.updates, &update));
    }
    body.push_str(&server_health(&server));
    body.push_str("<div class=\"stats\" aria-label=\"Server counts and storage\">");
    body.push_str(&super::html::stat(&counts.users.to_string(), "Accounts"));
    body.push_str(&super::html::stat(&counts.devices_active.to_string(), "Devices signed in"));
    body.push_str(&super::html::stat(&fmt_bytes(counts.stored_bytes), "Stored"));
    body.push_str(&super::html::stat(&fmt_bytes(counts.live_bytes), "Live documents"));
    body.push_str("</div>");
    body.push_str("<dl>");
    let mut row = |term: &str, value: String| {
        body.push_str(&format!("<dt>{}</dt><dd>{}</dd>", super::html::esc(term), super::html::esc(&value)));
    };
    row("Version", env!("CARGO_PKG_VERSION").to_owned());
    row("Storage", state.store.kind().to_owned());
    row("Database", state.meta.database_kind().to_owned());
    row("Your session ends", format!("{} at the latest", fmt_time(authed.session.absolute_expires_ms)));
    row("Second factor at sign-in", authed.session.mfa_verified_ms.map_or_else(|| "not asked".to_owned(), fmt_time));
    body.push_str("</dl><p>");
    body.push_str(&link("/web/users", "Manage users"));
    body.push_str(" &middot; ");
    body.push_str(&link("/web/spaces", "Manage team spaces"));
    body.push_str(" &middot; ");
    body.push_str(&link("/web/storage", "Manage storage"));
    body.push_str(" &middot; ");
    body.push_str(&link("/web/audit", "Read the audit log"));
    body.push_str("</p>");
    body.push_str(&format!(
        "<p>Storage: {} stored across {} accounts. {}</p>",
        fmt_bytes(storage.stored_bytes),
        storage.accounts,
        link("/web/storage", "View storage details")
    ));
    Ok(ok_page(&authed.ctx(), "Overview", &body))
}

fn update_banner(settings: &crate::updates::UpdateSettings, update: &crate::updates::UpdateInfo) -> String {
    let summary = if update.channel == "branch" {
        format!(
            "Newer commit available: {} (you run {})",
            update.latest.get(..7).unwrap_or(&update.latest),
            update.current.get(..7).unwrap_or(&update.current)
        )
    } else {
        format!("Update available: {} (you run {})", update.latest, update.current)
    };
    let command = format!("cd {} && ./packaging/docker/update.sh", settings.install_dir);
    let repo = settings.repo.split('/').map(super::html::url_encode).collect::<Vec<_>>().join("/");
    let href = if update.channel == "branch" {
        format!("https://github.com/{repo}/compare/{}...{}", update.current, update.latest)
    } else {
        format!("https://github.com/{repo}/releases/tag/{}", super::html::url_encode(&update.latest))
    };
    format!(
        "<section class=\"notice warning update-banner\"><p>{}</p><p>{}</p><pre><code>{}</code></pre><p>This script backs up first, then pulls, rebuilds and restarts. Read the release notes first.</p></section>",
        super::html::esc(&summary),
        super::html::link(&href, "View update details"),
        super::html::esc(&command)
    )
}

// ---- users ----

fn status_cell(user: &AdminUser) -> String {
    let mut parts = Vec::new();
    match (user.removed_ms, user.disabled_ms) {
        (Some(ms), _) => parts.push(super::html::badge(&format!("removed {}", fmt_time(ms)), "disabled")),
        (None, Some(ms)) => parts.push(super::html::badge(&format!("disabled {}", fmt_time(ms)), "disabled")),
        (None, None) => parts.push(super::html::badge("active", "active")),
    }
    if user.two_factor_enabled {
        parts.push(super::html::badge("two-factor", "two-factor"));
    }
    if user.invite_pending {
        parts.push(super::html::badge("invite open", "invited"));
    }
    parts.join(" ")
}

/// `GET /web/users?query&cursor`.
pub(super) async fn users(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let authed = authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let q: UsersQuery = read_query(&request)?;
    let search = q.query.as_deref().filter(|s| !s.is_empty());
    if let Some(search) = search
        && (search.len() > MAX_QUERY_BYTES || search.chars().any(char::is_control))
    {
        return Err(WebError::invalid(format!("The search is at most {MAX_QUERY_BYTES} bytes of printable characters.")));
    }
    let after = q.cursor.as_deref().filter(|c| !c.is_empty()).map(decode_cursor).transpose()?;
    let page_of = state.meta.admin_list_users(after.as_deref(), search, DEFAULT_PAGE, state.now_ms()).await?;
    let ctx = authed.ctx();
    let two_factor = asks_for_code(&state, &authed).await?;

    let mut body = heading("Users");
    body.push_str(&notice_html(q.notice.as_deref()));
    body.push_str("<form method=\"get\" action=\"/web/users\" class=\"search\" role=\"search\">");
    body.push_str(&field("search", "query", "Find a username", "search", search.unwrap_or_default(), "maxlength=\"64\""));
    body.push_str("<p class=\"field\"><button type=\"submit\">Search</button></p></form>");
    let rows: Vec<Vec<String>> = page_of
        .users
        .iter()
        .map(|u| {
            vec![
                link(&user_href(u.id), &u.username),
                super::html::esc(u.role.as_str()),
                status_cell(u),
                u.devices_active.to_string(),
                super::html::esc(&fmt_bytes(u.stored_bytes)),
                super::html::esc(&u.quota_bytes.map_or_else(|| "none".to_owned(), fmt_bytes)),
            ]
        })
        .collect();
    if rows.is_empty() {
        body.push_str("<p>No accounts match.</p>");
    } else {
        body.push_str(&table("Accounts", &["Username", "Role", "Status", "Devices", "Stored", "Quota"], &rows));
    }
    body.push_str("<p class=\"pager\">");
    if q.cursor.as_deref().is_some_and(|c| !c.is_empty()) {
        body.push_str(&link(&format!("/web/users?query={}", url_encode(search.unwrap_or_default())), "First page"));
    }
    if let Some(next) = &page_of.next_after {
        let cursor = base64_cursor(next);
        body.push_str(&link(&format!("/web/users?query={}&cursor={}", url_encode(search.unwrap_or_default()), url_encode(&cursor)), "Next page"));
    }
    body.push_str("</p>");

    body.push_str("<h2 id=\"new\">New account</h2><p class=\"muted\">The account gets an invite code to hand to its owner. The owner chooses the password when redeeming it.</p>");
    let mut inner =
        field("create", "username", "Username", "text", "", "required maxlength=\"64\" pattern=\"[A-Za-z0-9._\\-]{1,64}\" autocomplete=\"off\"");
    inner.push_str(&select("create", "role", "Role", &[("member", "member"), ("admin", "admin")], "member"));
    inner.push_str(&confirm_fields("create", two_factor));
    inner.push_str("<p><button type=\"submit\">Create account</button></p>");
    body.push_str(&post_form(&ctx, "/web/users", "panel", &inner));
    Ok(ok_page(&ctx, "Users", &body))
}

/// The cursor of the JSON list (the last username of a page, in base64url), which `decode_cursor` reads back.
pub(super) fn base64_cursor(username: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(username.as_bytes())
}

fn device_rows(ctx: &Ctx, user: UserId, devices: &[DeviceRecord], two_factor: bool) -> Vec<Vec<String>> {
    devices
        .iter()
        .enumerate()
        .map(|(n, d)| {
            let client = [d.client.kind.as_str(), d.client.platform.as_str(), d.client.app_version.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            let state = match (d.revoked, d.revoked_ms, d.revoke_reason) {
                (true, Some(ms), Some(reason)) => format!("signed out {} ({})", fmt_time(ms), reason.as_str()),
                (true, _, _) => "signed out".to_owned(),
                _ => "signed in".to_owned(),
            };
            let action = if d.revoked {
                String::new()
            } else {
                let form = format!("revoke-{n}");
                let mut inner = confirm_fields(&form, two_factor);
                inner.push_str("<p><button type=\"submit\" class=\"danger\">Sign this device out</button></p>");
                post_form(ctx, &format!("/web/users/{}/devices/{}/revoke", user.0, d.id.as_uuid()), "panel", &inner)
            };
            vec![
                super::html::esc(&d.name),
                super::html::esc(&client),
                super::html::esc(&fmt_time(d.created_ms)),
                super::html::esc(&fmt_time(d.last_used_ms)),
                super::html::esc(&state),
                action,
            ]
        })
        .collect()
}

/// `GET /web/users/{id}`: one account, the changes an admin can make to it, and its devices.
pub(super) async fn user(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let authed = authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let target = parse_user(&id)?;
    let q: NoticeQuery = read_query(&request)?;
    let found = state.meta.admin_user(target, state.now_ms()).await?.ok_or_else(|| WebError::not_found("account"))?;
    let devices = state.meta.admin_devices(target).await?;
    let usage = if found.removed_ms.is_none() { Some(state.meta.admin_storage_estimate(target).await?) } else { None };
    let ctx = authed.ctx();
    let two_factor = asks_for_code(&state, &authed).await?;
    let base = user_href(target);

    let mut body = heading(&format!("Account {}", found.username));
    body.push_str(&notice_html(q.notice.as_deref()));
    body.push_str("<p>");
    body.push_str(&link("/web/users", "All users"));
    body.push_str("</p><dl>");
    let row = |body: &mut String, term: &str, value: String| {
        body.push_str(&format!("<dt>{}</dt><dd>{}</dd>", super::html::esc(term), super::html::esc(&value)));
    };
    row(&mut body, "Username", found.username.clone());
    row(&mut body, "ID", found.id.0.to_string());
    row(&mut body, "Role", found.role.as_str().to_owned());
    row(&mut body, "Created", fmt_time(found.created_ms));
    let (status, tone) = match (found.removed_ms, found.disabled_ms) {
        (Some(ms), _) => (format!("marked for removal {}", fmt_time(ms)), "disabled"),
        (None, Some(ms)) => (format!("disabled {}", fmt_time(ms)), "disabled"),
        (None, None) => ("active".to_owned(), "active"),
    };
    body.push_str(&format!("<dt>Status</dt><dd>{}</dd>", super::html::badge(&status, tone)));
    body.push_str(&format!(
        "<dt>Two-factor sign-in</dt><dd>{}</dd>",
        super::html::badge(if found.two_factor_enabled { "on" } else { "off" }, if found.two_factor_enabled { "two-factor" } else { "neutral" })
    ));
    row(&mut body, "Devices signed in", found.devices_active.to_string());
    row(&mut body, "Stored", fmt_bytes(found.stored_bytes));
    if let Some(usage) = usage {
        for (term, value) in usage_rows(&usage) {
            row(&mut body, term, value);
        }
    } else {
        row(&mut body, "Quota", found.quota_bytes.map_or_else(|| "none (the server's cap applies, if any)".to_owned(), fmt_bytes));
    }
    row(&mut body, "Invite", if found.invite_pending { "open".to_owned() } else { "none".to_owned() });
    body.push_str("</dl>");

    if found.removed_ms.is_some() {
        body.push_str(&notice("This account is marked for removal. It can not be changed any more.", false));
    } else {
        body.push_str("<h2>Role and quota</h2>");
        let mut inner = select("role", "role", "Role", &[("", "Keep as it is"), ("member", "member"), ("admin", "admin")], "");
        inner.push_str(&field("role", "quota_mib", "Quota in MiB (at least 4)", "number", "", "min=\"4\" step=\"1\" inputmode=\"numeric\""));
        inner.push_str(&checkbox("role", "remove_quota", "Remove the quota"));
        inner.push_str(&confirm_fields("role", two_factor));
        inner.push_str("<p><button type=\"submit\">Save</button></p>");
        body.push_str(&post_form(&ctx, &format!("{base}/update"), "panel", &inner));

        body.push_str("<h2>Access</h2>");
        let (verb, action, class, label) = if found.disabled_ms.is_some() {
            ("enable", "enable", "", "Enable the account")
        } else {
            ("disable", "disable", "danger", "Disable the account and sign its devices out")
        };
        let mut inner = confirm_fields(verb, two_factor);
        inner.push_str(&format!("<p><button type=\"submit\" class=\"{class}\">{label}</button></p>"));
        body.push_str(&post_form(&ctx, &format!("{base}/{action}"), "panel", &inner));
        let mut inner = confirm_fields("remove", two_factor);
        inner.push_str("<p class=\"muted\">Marks the account for removal. It leaves the lists and its devices are signed out. Nothing it stored is deleted, and its name stays taken.</p>");
        inner.push_str("<p><button type=\"submit\" class=\"danger\">Remove the account</button></p>");
        body.push_str(&post_form(&ctx, &format!("{base}/remove"), "panel", &inner));

        body.push_str("<h2>Invite and reset codes</h2><p class=\"muted\">A code is shown once, right after it is made. It is not kept anywhere.</p>");
        let mut inner = confirm_fields("invite", two_factor);
        inner.push_str("<p class=\"muted\">For an account that has no password yet.</p><p><button type=\"submit\">Make a new invite</button></p>");
        body.push_str(&post_form(&ctx, &format!("{base}/invite"), "panel", &inner));
        let mut inner = checkbox("reset", "reset_two_factor", "Also turn two-factor sign-in off when the code is redeemed");
        inner.push_str(&confirm_fields("reset", two_factor));
        inner.push_str("<p class=\"muted\">Good for an hour. Redeeming it sets a new password and signs every device out.</p><p><button type=\"submit\">Make a password reset code</button></p>");
        body.push_str(&post_form(&ctx, &format!("{base}/password-reset"), "panel", &inner));
        let mut inner = confirm_fields("end-code", two_factor);
        inner.push_str("<p><button type=\"submit\">End the open invite or reset code</button></p>");
        body.push_str(&post_form(&ctx, &format!("{base}/invite/revoke"), "panel", &inner));

        if found.two_factor_enabled {
            body.push_str("<h2>Two-factor sign-in</h2>");
            let mut inner = confirm_fields("reset2fa", two_factor);
            inner.push_str("<p class=\"muted\">For someone who lost their authenticator and their recovery codes. Every device of the account is signed out.</p>");
            inner.push_str("<p><button type=\"submit\" class=\"danger\">Turn the account's two-factor sign-in off</button></p>");
            body.push_str(&post_form(&ctx, &format!("{base}/reset-two-factor"), "panel", &inner));
        }
    }

    body.push_str("<h2>Devices</h2>");
    if devices.is_empty() {
        body.push_str("<p>This account has no devices.</p>");
    } else {
        body.push_str(&table(
            "Devices",
            &["Name", "Client", "Added", "Last used", "State", "Action"],
            &device_rows(&ctx, target, &devices, two_factor),
        ));
    }
    Ok(ok_page(&ctx, &format!("Account {}", found.username), &body))
}

// ---- changes ----

/// The start of every change: the session, the form, its CSRF token, in that order.
pub(super) async fn begin(state: &AppState, request: Request) -> Result<(Authed, ActionForm), WebError> {
    let authed = authenticate(state, request.headers()).await?;
    let form: ActionForm = read_form(state, request).await?;
    authed.check_csrf(&form.csrf)?;
    Ok((authed, form))
}

/// The acting admin after their confirmation, which is the JSON routes' ([`confirm_actor`]): `400` for a missing password,
/// `403` for a wrong password or code, `429` for the limits.
pub(super) async fn actor(state: &AppState, authed: &Authed, form: &ActionForm) -> Result<AdminActor, WebError> {
    let some = |text: &str| if text.is_empty() { None } else { Some(text.to_owned()) };
    let given = Confirmation::new(some(&form.password), some(&form.totp_code), some(&form.recovery_code));
    let via = AdminVia::Web { id_hash: authed.hash, idle_ms: state.web.as_ref().map_or(0, |web| web.idle_ms) };
    match confirm_actor(state, authed.session.user, &authed.session.username, via, given).await {
        Ok(actor) => Ok(actor),
        Err(e) => {
            // The confirmation's own limits don't say how long to wait; the pages do, from the windows they come from.
            let mut e = WebError::from(e);
            if e.status == StatusCode::TOO_MANY_REQUESTS && e.retry_after.is_none() {
                let key = format!("user:{}", authed.session.username);
                let now = tokio::time::Instant::now().into_std();
                e.retry_after = Some(super::retry_secs(&[(&state.password_changes, &key), (&state.second_factors, &key)], now).unwrap_or(60));
            }
            Err(e)
        }
    }
}

/// A code the admin just made, as the answer to the `POST`: shown once, on a page nothing keeps.
fn code_page(ctx: &Ctx, title: &str, intro: &str, code: &AdminCode, back: &str) -> Response {
    let mut body = heading(title);
    body.push_str(&notice(intro, false));
    body.push_str(&format!("<p><span class=\"secret\">{}</span></p>", super::html::esc(&code.code)));
    if let Some(url) = &code.join_url {
        body.push_str(&format!("<p>Or as a link: <code>{}</code></p>", super::html::esc(url)));
    }
    body.push_str(&format!("<p>It works until {}.</p>", super::html::esc(&fmt_time(code.expires_ms))));
    body.push_str("<p class=\"muted\">This is the only time the code is shown. Craft Cloud does not keep it. Reloading this page asks the browser to send the form again, which makes a new code.</p>");
    body.push_str(&format!("<p>{}</p>", link(back, "Done")));
    ok_page(ctx, title, &body)
}

fn admin_code(state: &AppState, code: craft_cloud_proto::join::Code, expires_ms: i64) -> AdminCode {
    crate::admin::admin_code(state, code, expires_ms)
}

/// `POST /web/users`: a new account and its invite.
pub(super) async fn create_user(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    if !valid_username(&form.username) {
        return Err(WebError::invalid("Usernames are 1-64 letters, digits, '.', '_' or '-'."));
    }
    let role = role_of(&form.role)?;
    let actor = actor(&state, &authed, &form).await?;
    let code = new_code()?;
    let (user, issued) = state.meta.admin_create_user(&actor, &form.username, role, &code.hash(), state.now_ms(), INVITE_TTL_MS).await?;
    tracing::info!(target = %user.id.0, "admin (web): account created");
    let shown = admin_code(&state, code, issued.expires_ms);
    let back = user_href(user.id);
    Ok(code_page(
        &authed.ctx(),
        "Account created",
        "The account is made. Hand this invite code to its owner, who chooses a password with it.",
        &shown,
        &back,
    ))
}

/// `POST /web/users/{id}/update`: a new role and/or quota.
pub(super) async fn update_user(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let role = if form.role.is_empty() { None } else { Some(role_of(&form.role)?) };
    let quota = match (form.quota_mib.trim(), form.remove_quota.is_empty()) {
        ("", true) => None,
        ("", false) => Some(None),
        (_, false) => return Err(WebError::invalid("Give a quota or remove it, not both.")),
        (mib, true) => {
            let bytes = mib
                .parse::<u64>()
                .ok()
                .and_then(|mib| mib.checked_mul(MIB))
                .ok_or_else(|| WebError::invalid("The quota is a whole number of MiB."))?;
            Some(Some(bytes))
        }
    };
    if role.is_none() && quota.is_none() {
        return Err(WebError::invalid("Give a role or a quota to change."));
    }
    let actor = actor(&state, &authed, &form).await?;
    state.meta.admin_update_user(&actor, target, role, quota, state.now_ms()).await?;
    tracing::info!(target = %target.0, role_changed = role.is_some(), quota_changed = quota.is_some(), "admin (web): account changed");
    Ok(redirect(&format!("{}?notice=updated", user_href(target))))
}

/// `POST /web/users/{id}/disable`.
pub(super) async fn disable_user(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let actor = actor(&state, &authed, &form).await?;
    let revoked = state.meta.admin_disable_user(&actor, target, state.now_ms()).await?;
    end_live_connections(&state, &revoked);
    crate::spaces::wake_spaces_of(&state, target).await;
    tracing::info!(target = %target.0, revoked = revoked.len(), "admin (web): account disabled");
    Ok(redirect(&format!("{}?notice=disabled", user_href(target))))
}

/// `POST /web/users/{id}/enable`.
pub(super) async fn enable_user(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let actor = actor(&state, &authed, &form).await?;
    state.meta.admin_enable_user(&actor, target, state.now_ms()).await?;
    tracing::info!(target = %target.0, "admin (web): account enabled");
    Ok(redirect(&format!("{}?notice=enabled", user_href(target))))
}

/// `POST /web/users/{id}/remove`.
pub(super) async fn remove_user(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let actor = actor(&state, &authed, &form).await?;
    let revoked = state.meta.admin_remove_user(&actor, target, state.now_ms()).await?;
    end_live_connections(&state, &revoked);
    crate::spaces::wake_spaces_of(&state, target).await;
    tracing::info!(target = %target.0, revoked = revoked.len(), "admin (web): account marked for removal");
    Ok(redirect("/web/users?notice=removed"))
}

/// `POST /web/users/{id}/invite`: a new invite, shown once.
pub(super) async fn issue_invite(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let actor = actor(&state, &authed, &form).await?;
    let code = new_code()?;
    let issued = state.meta.admin_issue_invite(&actor, target, &code.hash(), state.now_ms(), INVITE_TTL_MS).await?;
    tracing::info!(target = %target.0, "admin (web): invite issued");
    let shown = admin_code(&state, code, issued.expires_ms);
    Ok(code_page(
        &authed.ctx(),
        "New invite",
        "Hand this invite code to the owner of the account, who chooses a password with it.",
        &shown,
        &user_href(target),
    ))
}

/// `POST /web/users/{id}/invite/revoke`.
pub(super) async fn revoke_invite(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let actor = actor(&state, &authed, &form).await?;
    state.meta.admin_revoke_invite(&actor, target, state.now_ms()).await?;
    tracing::info!(target = %target.0, "admin (web): invite revoked");
    Ok(redirect(&format!("{}?notice=invite_revoked", user_href(target))))
}

/// `POST /web/users/{id}/password-reset`: a reset code, shown once.
pub(super) async fn issue_reset(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let reset_two_factor = !form.reset_two_factor.is_empty();
    let actor = actor(&state, &authed, &form).await?;
    let code = new_code()?;
    let issued = state.meta.admin_issue_reset(&actor, target, &code.hash(), reset_two_factor, state.now_ms(), RESET_TTL_MS).await?;
    tracing::info!(target = %target.0, reset_two_factor, "admin (web): password reset code issued");
    let shown = admin_code(&state, code, issued.expires_ms);
    Ok(code_page(
        &authed.ctx(),
        "Password reset code",
        "Hand this code to the owner of the account. Redeeming it sets a new password and signs every device out.",
        &shown,
        &user_href(target),
    ))
}

/// `POST /web/users/{id}/reset-two-factor`.
pub(super) async fn reset_two_factor(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let actor = actor(&state, &authed, &form).await?;
    let revoked = state.meta.admin_reset_two_factor(&actor, target, state.now_ms()).await?;
    end_live_connections(&state, &revoked);
    tracing::info!(target = %target.0, revoked = revoked.len(), "admin (web): two-factor sign-in reset");
    Ok(redirect(&format!("{}?notice=two_factor_reset", user_href(target))))
}

/// `POST /web/users/{id}/devices/{device}/revoke`.
pub(super) async fn revoke_device(
    State(state): State<Shared>,
    Path((id, device)): Path<(String, String)>,
    request: Request,
) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let target = parse_user(&id)?;
    let device = Uuid::parse_str(&device).map(DeviceId::from_uuid).map_err(|_| WebError::not_found("device"))?;
    let actor = actor(&state, &authed, &form).await?;
    state.meta.admin_revoke_device(&actor, target, device, state.now_ms()).await?;
    state.live.revoke(device);
    tracing::info!(target = %target.0, %device, "admin (web): device revoked");
    Ok(redirect(&format!("{}?notice=device_revoked", user_href(target))))
}

// ---- the audit log ----

fn audit_cells(event: &AuditEvent) -> Vec<String> {
    let esc = super::html::esc;
    let who = |id: Option<Uuid>, kind: &str| match id {
        Some(id) if kind == "user" => link(&format!("/web/users/{id}"), &id.to_string()),
        Some(id) => format!("<code>{}</code>", esc(&id.to_string())),
        None => "-".to_owned(),
    };
    vec![
        esc(&fmt_time(event.at_ms)),
        esc(event.action.as_str()),
        esc(event.outcome.as_str()),
        who(event.actor_user.map(|u| u.0), "user"),
        who(event.target_id, &event.target_kind),
        event.count.to_string(),
        format!("<code>{}</code>", esc(&event.details)),
    ]
}

/// `GET /web/audit?user&action&cursor`: the log, newest first, filtered, a page at a time.
pub(super) async fn audit(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let authed = authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let params: Params = read_query(&request)?;
    let (user, action) = filters(&params.nonempty())?;
    let after = params.cursor.as_deref().filter(|c| !c.is_empty()).map(crate::audit::decode_cursor).transpose()?;
    let query = AuditQuery { user, action, before_ms: None, until_ms: None, after, limit: AUDIT_PAGE + 1 };
    let mut events = state.meta.audit_list(&query).await?;
    let more = events.len() > AUDIT_PAGE as usize;
    events.truncate(AUDIT_PAGE as usize);
    let ctx = authed.ctx();

    let (user_text, action_text) = (params.user.as_deref().unwrap_or_default(), params.action.as_deref().unwrap_or_default());
    let mut body = heading("Audit log");
    body.push_str("<form method=\"get\" action=\"/web/audit\" class=\"search\">");
    body.push_str(&field("audit", "user", "Account ID", "text", user_text, "maxlength=\"36\" autocomplete=\"off\""));
    body.push_str(&field(
        "audit",
        "action",
        "Event, like auth.login, or a group, like auth.",
        "text",
        action_text,
        "maxlength=\"64\" autocomplete=\"off\"",
    ));
    body.push_str("<p class=\"field\"><button type=\"submit\">Filter</button></p></form>");
    let filter_query = format!("user={}&action={}", url_encode(user_text), url_encode(action_text));
    if events.is_empty() {
        body.push_str("<p>No events match.</p>");
    } else {
        let rows: Vec<Vec<String>> = events.iter().map(audit_cells).collect();
        body.push_str(&table("Security events, newest first", &["Time", "Event", "Outcome", "Actor", "Target", "Count", "Details"], &rows));
    }
    body.push_str("<p class=\"pager\">");
    if params.cursor.as_deref().is_some_and(|c| !c.is_empty()) {
        body.push_str(&link(&format!("/web/audit?{filter_query}"), "Newest events"));
    }
    if more && let Some(last) = events.last() {
        body.push_str(&link(&format!("/web/audit?{filter_query}&cursor={}", url_encode(&encode_cursor(last.cursor()))), "Older events"));
    }
    body.push_str("</p><p>");
    body.push_str(&link(&format!("/web/audit/export?{filter_query}"), "Download these filters as NDJSON"));
    body.push_str("</p><p class=\"muted\">Accounts are named by ID. The log never holds a password, a code or an address.</p>");
    Ok(ok_page(&ctx, "Audit log", &body))
}

/// `GET /web/audit/export?user&action`: the same download as the JSON route, for the browser. It only reads.
pub(super) async fn audit_export(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let params: Params = read_query(&request)?;
    Ok(export_response(&state, params.nonempty()).await?)
}

// ---- the sign-in forms ----

/// The sign-in form. It carries no CSRF token (there is no session yet to bind one to): the Origin check is what refuses a
/// sign-in that another site's page made the browser send.
pub(super) fn login_form() -> String {
    let mut inner = field(
        "login",
        "username",
        "Username",
        "text",
        "",
        "required maxlength=\"64\" autocomplete=\"username\" autocapitalize=\"none\" spellcheck=\"false\"",
    );
    inner.push_str(&field("login", "password", "Password", "password", "", "required maxlength=\"1024\" autocomplete=\"current-password\""));
    inner.push_str("<p><button type=\"submit\">Sign in</button></p>");
    format!(
        "<form method=\"post\" action=\"/web/login\" class=\"panel\">{inner}</form><p class=\"muted\">Only administrators can sign in to these pages.</p>"
    )
}

/// The second step: the challenge the first step made, in a hidden field, and the code.
pub(super) fn second_factor_form(challenge: &str) -> String {
    let mut inner = format!("<input type=\"hidden\" name=\"challenge\" value=\"{}\">", super::html::esc(challenge));
    inner.push_str("<p>This account needs a second factor. Enter the code from your authenticator app, or one of your recovery codes.</p>");
    inner.push_str(&field(
        "second",
        "totp_code",
        "Code from your authenticator app",
        "text",
        "",
        "inputmode=\"numeric\" autocomplete=\"one-time-code\" pattern=\"[0-9]{6}\" maxlength=\"6\"",
    ));
    inner.push_str(&field("second", "recovery_code", "Or a recovery code", "text", "", "autocomplete=\"off\" maxlength=\"32\" spellcheck=\"false\""));
    inner.push_str("<p><button type=\"submit\">Continue</button></p>");
    format!("<form method=\"post\" action=\"/web/login/second-factor\" class=\"panel\">{inner}</form>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_forms_of_the_changes_print_nothing() {
        let form = ActionForm {
            password: "hunter2hunter2".into(),
            totp_code: "123456".into(),
            recovery_code: "ABCD".into(),
            username: "ana".into(),
            ..ActionForm::default()
        };
        let shown = format!("{form:?}");
        assert!(!shown.contains("hunter2") && !shown.contains("123456") && !shown.contains("ABCD") && !shown.contains("ana"), "{shown}");
    }

    #[test]
    fn only_known_notices_have_words() {
        assert_eq!(notice_text(Some("updated")), Some("The account was updated."));
        assert_eq!(notice_text(Some("<script>")), None);
        assert_eq!(notice_text(None), None);
        assert_eq!(notice_html(Some("nope")), "");
    }

    #[test]
    fn storage_overview_escapes_top_account_names() {
        let storage = StorageOverview {
            accounts: 1,
            stored_bytes: 12,
            default_quota_bytes: Some(100),
            accounts_with_override: 1,
            accounts_near_quota: 0,
            accounts_over_quota: 0,
            top_accounts: vec![craft_cloud_meta::StorageAccount {
                id: UserId(Uuid::nil()),
                username: "<script>alert('x')</script>".into(),
                stored_bytes: 12,
                quota_bytes: Some(100),
            }],
            manifest_index: craft_cloud_meta::StorageIndexCounts { complete: 1, building: 0, not_indexed: 0 },
        };
        let html = storage_section(&storage);
        assert!(html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt;"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn storage_details_escape_names_and_show_unconfigured_measurements() {
        let mut storage = AdminStorageOverview { capacity_bytes: None, store_kind: Some("s3".into()), ..Default::default() };
        storage.top_accounts.push(craft_cloud_proto::api::AdminStorageAccount {
            id: Uuid::nil(),
            username: "<script>bad()</script>".into(),
            stored_bytes: 1024,
            quota_bytes: None,
        });
        let html = storage_details(&storage);
        assert!(html.contains("not configured") && html.contains("not measured"), "{html}");
        assert!(html.contains("&lt;script&gt;bad()&lt;/script&gt;"), "{html}");
        assert!(html.contains("/web/users/00000000-0000-0000-0000-000000000000"), "{html}");
        assert!(!html.contains("<script>"), "{html}");
    }

    #[test]
    fn overview_health_only_says_healthy_when_no_fault_is_reported() {
        let mut info = AdminServerInfo {
            version: "test".into(),
            users: 0,
            devices_active: 0,
            stored_bytes: 0,
            live_bytes: 0,
            storage_kind: "disk".into(),
            database_kind: "sqlite".into(),
            started_ms: None,
            database_ok: Some(true),
            store_ok: Some(true),
            store_check_ms: None,
            refusing_uploads: Some(false),
        };
        assert!(server_health(&info).contains("Healthy"));
        info.refusing_uploads = Some(true);
        assert!(server_health(&info).contains("Refusing uploads"));
    }

    #[test]
    fn account_usage_rows_show_known_numbers_and_label_unmeasured_fields() {
        let usage = StorageEstimate { stored_bytes: 2048, quota_bytes: Some(4096), trash_bytes: Some(512), history_bytes: Some(256), reason: None };
        let rows = usage_rows(&usage);
        assert!(rows.contains(&("Usage stored", "2.0 KiB".to_owned())));
        assert!(rows.contains(&("Effective quota", "4.0 KiB".to_owned())));
        assert!(rows.contains(&("Physical", "not measured".to_owned())));
        assert!(rows.contains(&("Files", "not measured".to_owned())));
    }
}
