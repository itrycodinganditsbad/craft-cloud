//! The Team spaces page: the list (search, paging), creating a space, and one space's page with its members (read only here),
//! its limit, its archive state and the account that is named its owner. Each change is a form that posts to a route of its own,
//! is read and checked, and handed to the same `craft_cloud_meta` call as the JSON `/admin/spaces` route, with the same
//! confirmation and the web session as the acting administrator ([`craft_cloud_meta::AdminVia::Web`], checked again in the
//! transaction). After a change the space's long-polls look at their access again, as after the JSON routes' changes.
//!
//! The pages show counts, names and sizes, never the content of a space. No administrator gets access to the files by being
//! one: naming an owner is how an administrator gets a space a person to look after it.

use axum::extract::{Path, Request, State};
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_cloud_meta::{SpaceActor, SpaceOwner, SpaceSummary, UserId, valid_username};
use craft_cloud_proto::{SpaceId, SpaceName};
use serde::Deserialize;
use uuid::Uuid;

use super::html::{checkbox, confirm_fields, field, fmt_bytes, fmt_time, heading, link, notice, post_form, table, url_encode};
use super::pages::{ActionForm, MIB, actor, asks_for_code, base64_cursor, begin, ok_page};
use super::{Shared, WebError, authenticate, read_query, redirect};
use crate::admin::{DEFAULT_PAGE, MAX_QUERY_BYTES};
use crate::state::AppState;

#[derive(Deserialize, Default)]
struct SpacesQuery {
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
        "created" => Some("The team space was created."),
        "quota" => Some("The space's limit was changed."),
        "archived" => Some("The space is archived: every member can read it and nobody can change it. Nothing was deleted."),
        "unarchived" => Some("The space is active again."),
        "owner" => Some("The account is an owner of the space."),
        _ => None,
    }
}

fn notice_html(key: Option<&str>) -> String {
    notice_text(key).map(|text| notice(text, false)).unwrap_or_default()
}

fn space_href(id: SpaceId) -> String {
    format!("/web/spaces/{}", id.as_uuid())
}

fn user_href(id: UserId) -> String {
    format!("/web/users/{}", id.0)
}

fn parse_space(id: &str) -> Result<SpaceId, WebError> {
    Uuid::parse_str(id).map(SpaceId::from_uuid).map_err(|_| WebError::not_found("team space"))
}

fn quota_text(quota: Option<u64>) -> String {
    quota.map_or_else(|| "none".to_owned(), fmt_bytes)
}

fn state_text(space: &SpaceSummary) -> String {
    space.archived_ms.map_or_else(|| "active".to_owned(), |ms| format!("archived {}", fmt_time(ms)))
}

/// An owner as a list shows them: the account, and its state when it cannot sign in (it then has no access to the space).
fn owner_html(owner: &SpaceOwner) -> String {
    let state = match (owner.removed_ms, owner.disabled_ms) {
        (Some(_), _) => " (marked for removal)",
        (None, Some(_)) => " (disabled)",
        (None, None) => "",
    };
    format!("{}{}", link(&user_href(owner.user), &owner.username), super::html::esc(state))
}

/// The limit a form asks for: whole MiB, or none (`Some(None)` removes it). Neither is `None`; both is a mistake.
fn quota_from(form: &ActionForm) -> Result<Option<Option<u64>>, WebError> {
    match (form.quota_mib.trim(), form.remove_quota.is_empty()) {
        ("", true) => Ok(None),
        ("", false) => Ok(Some(None)),
        (_, false) => Err(WebError::invalid("Give a limit or remove it, not both.")),
        (mib, true) => {
            let bytes = mib
                .parse::<u64>()
                .ok()
                .and_then(|mib| mib.checked_mul(MIB))
                .ok_or_else(|| WebError::invalid("The limit is a whole number of MiB."))?;
            Ok(Some(Some(bytes)))
        }
    }
}

/// The owner a form names, by account name. An account that does not exist is `404`; one that is disabled or marked for removal
/// is refused by the store as for the JSON routes.
async fn owner_named(state: &AppState, form: &ActionForm) -> Result<UserId, WebError> {
    if !valid_username(&form.owner) {
        return Err(WebError::invalid("Usernames are 1-64 letters, digits, '.', '_' or '-'."));
    }
    let user = state.meta.user_by_name(&form.owner).await?.ok_or_else(|| WebError::not_found("account"))?;
    Ok(user.id)
}

/// The cursor of the list (the last name key of a page, in base64url).
fn decode_cursor(cursor: &str) -> Result<String, WebError> {
    let bad = || WebError::invalid("The cursor is not one this server gave.");
    String::from_utf8(URL_SAFE_NO_PAD.decode(cursor).map_err(|_| bad())?).map_err(|_| bad())
}

// ---- the list ----

/// `GET /web/spaces?query&cursor`: the team spaces by name, with their owners, members, use and state; a form to make one.
pub(super) async fn spaces(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let authed = authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let q: SpacesQuery = read_query(&request)?;
    let search = q.query.as_deref().filter(|s| !s.is_empty());
    if let Some(search) = search
        && (search.len() > MAX_QUERY_BYTES || search.chars().any(char::is_control))
    {
        return Err(WebError::invalid(format!("The search is at most {MAX_QUERY_BYTES} bytes of printable characters.")));
    }
    let after = q.cursor.as_deref().filter(|c| !c.is_empty()).map(decode_cursor).transpose()?;
    let page_of = state.meta.admin_list_spaces(after.as_deref(), search, DEFAULT_PAGE).await?;
    let ctx = authed.ctx();
    let two_factor = asks_for_code(&state, &authed).await?;

    let mut body = heading("Team spaces");
    body.push_str(&notice_html(q.notice.as_deref()));
    body.push_str("<form method=\"get\" action=\"/web/spaces\" class=\"search\" role=\"search\">");
    body.push_str(&field("search", "query", "Find a space by name", "search", search.unwrap_or_default(), "maxlength=\"64\""));
    body.push_str("<p class=\"field\"><button type=\"submit\">Search</button></p></form>");
    // The owners of the whole page in one query.
    let ids: Vec<SpaceId> = page_of.spaces.iter().map(|space| space.id).collect();
    let owners = state.meta.space_owners(&ids).await?;
    let mut rows = Vec::with_capacity(page_of.spaces.len());
    for space in &page_of.spaces {
        let mut cell: Vec<String> = owners.iter().filter(|owner| owner.space == space.id).map(owner_html).collect();
        if space.owners == 0 {
            cell.push(super::html::esc("no active owner"));
        }
        rows.push(vec![
            link(&space_href(space.id), &space.name),
            cell.join(", "),
            space.members.to_string(),
            super::html::esc(&format!("{} of {}", fmt_bytes(space.stored_bytes), quota_text(space.quota_bytes))),
            super::html::esc(&state_text(space)),
        ]);
    }
    if rows.is_empty() {
        body.push_str("<p>No team spaces match.</p>");
    } else {
        body.push_str(&table("Team spaces", &["Name", "Owners", "Members", "Used of limit", "State"], &rows));
    }
    body.push_str("<p class=\"pager\">");
    if q.cursor.as_deref().is_some_and(|c| !c.is_empty()) {
        body.push_str(&link(&format!("/web/spaces?query={}", url_encode(search.unwrap_or_default())), "First page"));
    }
    if let Some(next) = &page_of.next_after {
        body.push_str(&link(
            &format!("/web/spaces?query={}&cursor={}", url_encode(search.unwrap_or_default()), url_encode(&base64_cursor(next))),
            "Next page",
        ));
    }
    body.push_str("</p>");

    body.push_str("<h2 id=\"new\">New team space</h2><p class=\"muted\">The owner joins at once and invites the other members. Nobody else gets access to the files, administrators included.</p>");
    let mut inner = field("create", "name", "Name", "text", "", "required maxlength=\"64\" autocomplete=\"off\"");
    inner.push_str(&field(
        "create",
        "owner",
        "First owner (account name)",
        "text",
        "",
        "required maxlength=\"64\" autocomplete=\"off\" autocapitalize=\"none\" spellcheck=\"false\"",
    ));
    inner.push_str(&field(
        "create",
        "quota_mib",
        "Limit in MiB (at least 4, empty for none)",
        "number",
        "",
        "min=\"4\" step=\"1\" inputmode=\"numeric\"",
    ));
    inner.push_str(&confirm_fields("create", two_factor));
    inner.push_str("<p><button type=\"submit\">Create team space</button></p>");
    body.push_str(&post_form(&ctx, "/web/spaces", "panel", &inner));
    Ok(ok_page(&ctx, "Team spaces", &body))
}

/// `POST /web/spaces`: a team space and its first owner.
pub(super) async fn create_space(State(state): State<Shared>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let name = SpaceName::new(&form.name).map_err(|e| WebError::invalid(format!("A space's name: {e}")))?;
    let quota = match quota_from(&form)? {
        None => None,
        Some(None) => return Err(WebError::invalid("A new space has no limit to remove; leave the field empty.")),
        Some(Some(bytes)) => Some(bytes),
    };
    // The owner is looked up before the confirmation, so a mistyped name does not use up a code (the store checks it again).
    let owner = owner_named(&state, &form).await?;
    let admin = actor(&state, &authed, &form).await?;
    let space = state.meta.create_team_space(&SpaceActor::Admin(admin), &name, owner, quota, state.now_ms()).await?;
    tracing::info!(%space, "admin (web): team space created");
    Ok(redirect(&format!("{}?notice=created", space_href(space))))
}

// ---- one space ----

/// `GET /web/spaces/{id}`: the space, its members (read only), and the changes an administrator can make to it.
pub(super) async fn space(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let authed = authenticate(&state, request.headers()).await.map_err(WebError::for_get)?;
    let id = parse_space(&id)?;
    let q: NoticeQuery = read_query(&request)?;
    let found = match state.meta.admin_space(id).await {
        Ok(found) => found,
        Err(craft_cloud_meta::MetaError::NotFound(_)) => return Err(WebError::not_found("team space")),
        Err(e) => return Err(e.into()),
    };
    let members = state.meta.list_members(id).await?;
    let owners = state.meta.space_owners(&[id]).await?;
    let ctx = authed.ctx();
    let two_factor = asks_for_code(&state, &authed).await?;
    let base = space_href(id);

    let mut body = heading(&format!("Team space {}", found.name));
    body.push_str(&notice_html(q.notice.as_deref()));
    body.push_str("<p>");
    body.push_str(&link("/web/spaces", "All team spaces"));
    body.push_str("</p><dl>");
    let mut row = |term: &str, value: String| {
        body.push_str(&format!("<dt>{}</dt><dd>{}</dd>", super::html::esc(term), super::html::esc(&value)));
    };
    row("Name", found.name.clone());
    row("ID", id.as_uuid().to_string());
    row("Created", fmt_time(found.created_ms));
    row("State", state_text(&found));
    row("Members", found.members.to_string());
    row("Owners who can sign in", found.owners.to_string());
    row("Stored", fmt_bytes(found.stored_bytes));
    row("Limit", quota_text(found.quota_bytes));
    body.push_str("</dl>");
    if found.owners == 0 {
        body.push_str(&notice("This space has no owner who can sign in, so nobody can manage it. Name one below.", true));
    }

    body.push_str("<h2>Members</h2>");
    if members.is_empty() {
        body.push_str("<p>This space has no members.</p>");
    } else {
        let rows: Vec<Vec<String>> = members
            .iter()
            .map(|m| {
                let account = owners.iter().find(|owner| owner.user == m.user).map_or("", |owner| match (owner.removed_ms, owner.disabled_ms) {
                    (Some(_), _) => ", account marked for removal",
                    (None, Some(_)) => ", account disabled",
                    (None, None) => "",
                });
                vec![
                    link(&user_href(m.user), &m.username),
                    super::html::esc(m.role.as_str()),
                    super::html::esc(&format!("{}{account}", if m.accepted() { "member" } else { "invited" })),
                ]
            })
            .collect();
        body.push_str(&table("Members", &["Account", "Role", "State"], &rows));
    }
    body.push_str("<p class=\"muted\">Owners invite and remove members and change their roles from the Craft Cloud app.</p>");

    body.push_str("<h2>Limit</h2>");
    let mut inner = field("quota", "quota_mib", "Limit in MiB (at least 4)", "number", "", "min=\"4\" step=\"1\" inputmode=\"numeric\"");
    inner.push_str(&checkbox("quota", "remove_quota", "Remove the limit"));
    inner.push_str(&confirm_fields("quota", two_factor));
    inner.push_str("<p><button type=\"submit\">Save the limit</button></p>");
    body.push_str(&post_form(&ctx, &format!("{base}/quota"), "panel", &inner));

    body.push_str("<h2>Archive</h2>");
    let (action, class, label, note) = if found.archived_ms.is_some() {
        ("unarchive", "", "Make the space active again", "Members get the access their roles give again.")
    } else {
        (
            "archive",
            "danger",
            "Archive the space",
            "Every member, owners included, can only read it while it is archived. Nothing is deleted. It leaves the lists of all but its owners.",
        )
    };
    let mut inner = confirm_fields(action, two_factor);
    inner.push_str(&format!("<p class=\"muted\">{note}</p><p><button type=\"submit\" class=\"{class}\">{label}</button></p>"));
    body.push_str(&post_form(&ctx, &format!("{base}/{action}"), "panel", &inner));

    body.push_str("<h2>Owner</h2>");
    let mut inner = field(
        "owner",
        "owner",
        "Account name",
        "text",
        "",
        "required maxlength=\"64\" autocomplete=\"off\" autocapitalize=\"none\" spellcheck=\"false\"",
    );
    inner.push_str(&confirm_fields("owner", two_factor));
    inner.push_str("<p class=\"muted\">The account becomes an owner at once, without an invitation to accept, whether it is a member, invited or neither. Use this when the owners are gone. Other owners stay owners.</p>");
    inner.push_str("<p><button type=\"submit\" class=\"danger\">Name this owner</button></p>");
    body.push_str(&post_form(&ctx, &format!("{base}/owner"), "panel", &inner));
    Ok(ok_page(&ctx, &format!("Team space {}", found.name), &body))
}

// ---- changes ----

/// `POST /web/spaces/{id}/quota`: a new limit, or none.
pub(super) async fn set_quota(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let id = parse_space(&id)?;
    let quota = quota_from(&form)?.ok_or_else(|| WebError::invalid("Give a limit or remove it."))?;
    let admin = actor(&state, &authed, &form).await?;
    state.meta.set_space_quota(&admin, id, quota, state.now_ms()).await?;
    crate::spaces::changed(&state, id);
    tracing::info!(space = %id, "admin (web): team space limit changed");
    Ok(redirect(&format!("{}?notice=quota", space_href(id))))
}

async fn archive_to(state: &AppState, id: &str, request: Request, archived: bool) -> Result<Response, WebError> {
    let (authed, form) = begin(state, request).await?;
    let id = parse_space(id)?;
    let admin = actor(state, &authed, &form).await?;
    state.meta.set_space_archived(&SpaceActor::Admin(admin), id, archived, state.now_ms()).await?;
    crate::spaces::changed(state, id);
    tracing::info!(space = %id, archived, "admin (web): team space archive state changed");
    Ok(redirect(&format!("{}?notice={}", space_href(id), if archived { "archived" } else { "unarchived" })))
}

/// `POST /web/spaces/{id}/archive`.
pub(super) async fn archive(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    archive_to(&state, &id, request, true).await
}

/// `POST /web/spaces/{id}/unarchive`.
pub(super) async fn unarchive(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    archive_to(&state, &id, request, false).await
}

/// `POST /web/spaces/{id}/owner`: an account named as an accepted owner.
pub(super) async fn set_owner(State(state): State<Shared>, Path(id): Path<String>, request: Request) -> Result<Response, WebError> {
    let (authed, form) = begin(&state, request).await?;
    let id = parse_space(&id)?;
    let owner = owner_named(&state, &form).await?;
    let admin = actor(&state, &authed, &form).await?;
    state.meta.set_owner_accepted(&admin, id, owner, state.now_ms()).await?;
    crate::spaces::changed(&state, id);
    tracing::info!(space = %id, "admin (web): team space owner named");
    Ok(redirect(&format!("{}?notice=owner", space_href(id))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_known_notices_have_words() {
        assert!(notice_text(Some("created")).is_some());
        assert_eq!(notice_text(Some("<script>")), None);
        assert_eq!(notice_text(None), None);
        assert_eq!(notice_html(Some("nope")), "");
    }

    #[test]
    fn a_limit_is_whole_mib_and_not_both_ways() {
        let form = |mib: &str, remove: &str| ActionForm { quota_mib: mib.into(), remove_quota: remove.into(), ..ActionForm::default() };
        assert!(matches!(quota_from(&form("", "")), Ok(None)));
        assert!(matches!(quota_from(&form("", "on")), Ok(Some(None))));
        assert!(matches!(quota_from(&form(" 8 ", "")), Ok(Some(Some(bytes))) if bytes == 8 * MIB));
        for bad in ["x", "-1", "1.5", "99999999999999999999", "18446744073709551615"] {
            assert!(quota_from(&form(bad, "")).is_err(), "{bad}");
        }
        assert!(quota_from(&form("8", "on")).is_err());
    }

    #[test]
    fn a_cursor_is_base64_of_a_name_key() {
        assert_eq!(decode_cursor(&base64_cursor("studio")).ok().as_deref(), Some("studio"));
        assert!(decode_cursor("not base64!").is_err());
    }
}
