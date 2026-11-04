//! The security audit log over HTTP: `GET /api/v1/account/activity` (the signed-in account's own events), and for
//! administrators `GET /api/v1/admin/audit` (the whole log, filtered) and `GET /api/v1/admin/audit/export` (the same as
//! NDJSON, streamed).
//!
//! - **Events are written by the changes they record**, in the transaction of the change (`craft_cloud_meta::audit`).
//!   This module only reads, and writes the one event that is not part of a change: a failed sign-in
//!   ([`login_failed`]), after the answer is decided, whose failure to be written never changes the answer.
//! - **Pages** are newest first in the order `(at_ms, id)` descending. `cursor` is opaque (base64url of that pair) and
//!   exclusive: the next page starts strictly behind it, so events with one timestamp are neither repeated nor lost.
//!   A cursor this server did not make is `400 invalid`. `cursor` wins over `before_ms`, which only limits a listing's
//!   first call. `limit` over the route's most is `400 invalid` rather than quietly smaller.
//! - **The account's own activity** holds the events the account acted in or was the target of, and shows the device that
//!   acted only when it is the account's own.
//! - **The export** is bounded in every way: pages of [`AuditExportLimits::page`] rows per query, a channel of
//!   [`AuditExportLimits::channel`] pages between the database and the connection (so a slow client slows the reading
//!   and does not fill memory), the end of the log fixed when it starts, at most [`AuditExportLimits::max_rows`] event
//!   lines. The last line says how it ended: `{"complete":true,...}`, or `{"truncated":true,...}` when there were more
//!   events than it holds. A database error in the middle ends the connection with an error and no last line, and a
//!   client that goes away stops the reading at once. A client counts an export as whole only with the `complete` line.
//! - **Nothing is logged of the events.** A failure to write or read one is logged without its content.

use std::future::Future;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::rejection::QueryRejection;
use axum::extract::{Extension, Query, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, middleware};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use craft_cloud_meta::{ActionFilter, AuditCursor, AuditEvent, AuditFailure, AuditQuery, DeviceAuth, MetaError, Step, UserId};
use craft_cloud_proto::api::{AuditDetails, AuditExportEnd, AuditPage};
use serde::Deserialize;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::require_admin;
use crate::error::ApiError;
use crate::state::AppState;

type Shared = Arc<AppState>;

/// Events one page of the account's activity holds when `limit` is not given, and the most it may hold.
const DEFAULT_PAGE: u32 = 50;
const ACCOUNT_MAX_PAGE: u32 = 100;
/// The same for the admin's list.
const ADMIN_MAX_PAGE: u32 = 200;
/// How an export is offered: always under this name, whatever the filters.
const EXPORT_DISPOSITION: &str = "attachment; filename=\"craft-cloud-audit.ndjson\"";

/// How an export is cut. The defaults are the contract in `docs/design/sync.md`; tests make them smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditExportLimits {
    /// Rows one database query reads.
    pub page: u32,
    /// Pages the channel to the connection holds.
    pub channel: usize,
    /// Event lines in an export; more events than that make it `truncated`.
    pub max_rows: u64,
}

impl Default for AuditExportLimits {
    fn default() -> Self {
        Self { page: 500, channel: 2, max_rows: 100_000 }
    }
}

/// The admin routes, behind `require_admin`; `crate::api::router` merges them into the signed-in routes, so
/// `require_device` runs first.
pub fn admin_routes() -> Router<Shared> {
    Router::new()
        .route("/admin/audit", get(admin_list))
        .route("/admin/audit/export", get(admin_export))
        .route_layer(middleware::from_fn(require_admin))
}

/// Writes the event of a sign-in that failed at its password, in a transaction of its own. `target` is the account the
/// guess was about, `None` for a name no account has (the log never holds a name that was typed). A failure is logged and
/// otherwise ignored: the answer to the client and the limits around it do not depend on the log.
pub(crate) async fn login_failed(state: &AppState, target: Option<UserId>) {
    let failure = AuditFailure { action: craft_cloud_meta::AuditAction::AuthLoginFailed, target, step: Step::Password };
    if let Err(e) = state.meta.audit_record_failure(failure, state.now_ms()).await {
        tracing::warn!(error = %e, "couldn't write an audit event");
    }
}

/// Writes the event of a sign-in whose challenge was not good (unknown, expired, used, or out of tries), as
/// [`login_failed`] does. `target` is the account when the challenge was found, `None` when it was not (nothing a client sent
/// is stored). The wrong second factors themselves are written by the store, where the account is known.
pub(crate) async fn challenge_failed(state: &AppState, target: Option<UserId>) {
    let failure = AuditFailure { action: craft_cloud_meta::AuditAction::AuthSecondFactorFailed, target, step: Step::ChallengeExpired };
    if let Err(e) = state.meta.audit_record_failure(failure, state.now_ms()).await {
        tracing::warn!(error = %e, "couldn't write an audit event");
    }
}

// ---- reading parameters ----

/// The query of the two lists and the export. Unknown parameters are ignored, as everywhere in the API.
#[derive(Debug, Deserialize)]
pub(crate) struct Params {
    cursor: Option<String>,
    before_ms: Option<i64>,
    limit: Option<u32>,
    user: Option<String>,
    action: Option<String>,
}

fn bad_cursor() -> ApiError {
    ApiError::invalid("the cursor is not one this server gave")
}

fn encode_cursor(cursor: AuditCursor) -> String {
    URL_SAFE_NO_PAD.encode(cursor.to_bytes())
}

fn decode_cursor(text: &str) -> Result<AuditCursor, ApiError> {
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| bad_cursor())?;
    AuditCursor::from_bytes(&bytes).ok_or_else(bad_cursor)
}

/// The limit of a list: 1 to `max`, `DEFAULT_PAGE` when not given.
fn page_limit(limit: Option<u32>, max: u32) -> Result<u32, ApiError> {
    let limit = limit.unwrap_or(DEFAULT_PAGE);
    if (1..=max).contains(&limit) { Ok(limit) } else { Err(ApiError::invalid(format!("limit is 1 to {max}"))) }
}

/// The `user` and `action` filters. A `user` is an account ID, an `action` one of the names or a prefix of them ending
/// in a dot like `auth.`; anything else is `400 invalid`, so nothing typed reaches a query.
fn filters(params: &Params) -> Result<(Option<UserId>, Option<ActionFilter>), ApiError> {
    let user =
        params.user.as_deref().map(|text| Uuid::parse_str(text).map(UserId).map_err(|_| ApiError::invalid("user is an account ID"))).transpose()?;
    let action = params
        .action
        .as_deref()
        .map(|text| ActionFilter::parse(text).ok_or_else(|| ApiError::invalid("action is an event name like auth.login, or a prefix like auth.")))
        .transpose()?;
    Ok((user, action))
}

/// The event as the API shows it. `own`: an account whose own activity this is, which sees the device only of its own acts, and
/// the details of an event about another account (a former administrator's past act on someone else) not at all.
fn wire(event: AuditEvent, own: Option<UserId>) -> craft_cloud_proto::api::AuditEvent {
    let device_visible = own.is_none_or(|own| event.actor_user == Some(own));
    let about_another = own.is_some_and(|own| event.target_kind == "user" && event.target_id.is_some_and(|t| t != own.0));
    craft_cloud_proto::api::AuditEvent {
        id: event.id,
        at_ms: event.at_ms,
        actor_user: event.actor_user.map(|u| u.0),
        actor_device: if device_visible { event.actor_device } else { None },
        action: event.action.as_str().to_owned(),
        target_kind: event.target_kind,
        target_id: event.target_id,
        outcome: event.outcome.as_str().to_owned(),
        // Written by this code from closed lists; a row that does not read as such shows no details rather than failing the page.
        details: if about_another { AuditDetails::default() } else { serde_json::from_str::<AuditDetails>(&event.details).unwrap_or_default() },
        count: event.count,
    }
}

/// One page: asks for one more event than `limit` to know whether there is a next one.
async fn page(state: &AppState, params: &Params, max: u32, restrict_to: Option<UserId>) -> Result<Json<AuditPage>, ApiError> {
    let limit = page_limit(params.limit, max)?;
    let (user, action) = filters(params)?;
    if params.before_ms.is_some_and(|before| before < 0) {
        return Err(ApiError::invalid("before_ms is a time in milliseconds"));
    }
    let after = params.cursor.as_deref().map(decode_cursor).transpose()?;
    let query = AuditQuery {
        // An account sees nobody's events but its own, whatever the request names.
        user: restrict_to.or(user),
        action,
        // The cursor is a position and wins over the time.
        before_ms: if after.is_some() { None } else { params.before_ms },
        until_ms: None,
        after,
        limit: limit.saturating_add(1),
    };
    let mut events = state.meta.audit_list(&query).await?;
    let more = events.len() > limit as usize;
    events.truncate(limit as usize);
    let next = if more { events.last().map(|event| encode_cursor(event.cursor())) } else { None };
    Ok(Json(AuditPage { events: events.into_iter().map(|event| wire(event, restrict_to)).collect(), next }))
}

/// `GET /api/v1/account/activity?cursor&before_ms&limit`: the signed-in account's own security events.
pub(crate) async fn account_activity(
    State(state): State<Shared>,
    Extension(auth): Extension<DeviceAuth>,
    params: Result<Query<Params>, QueryRejection>,
) -> Result<Json<AuditPage>, ApiError> {
    let Query(params) = params?;
    // The account sees events about itself; `user` and `action` are the admin's filters and are not read here.
    let own = Params { user: None, action: None, ..params };
    page(&state, &own, ACCOUNT_MAX_PAGE, Some(auth.user)).await
}

/// `GET /api/v1/admin/audit?cursor&before_ms&limit&user&action`.
async fn admin_list(State(state): State<Shared>, params: Result<Query<Params>, QueryRejection>) -> Result<Json<AuditPage>, ApiError> {
    let Query(params) = params?;
    page(&state, &params, ADMIN_MAX_PAGE, None).await
}

// ---- the export ----

/// One page of the export's reading: the events strictly behind `after`, at most `rows`.
type Fetch<'a> =
    Box<dyn FnMut(Option<AuditCursor>, u32) -> std::pin::Pin<Box<dyn Future<Output = Result<Vec<AuditEvent>, MetaError>> + Send + 'a>> + Send + 'a>;

/// Reads the log in pages with `fetch` and sends NDJSON to `tx`, a page at a time, ending with the line that says how it
/// ended. Returns without a last line when `fetch` fails (the error is sent, so the connection ends in error) or when the
/// receiver is gone (the reading stops there and then: it also stops a query that is under way).
async fn export_pages(mut fetch: Fetch<'_>, limits: AuditExportLimits, tx: mpsc::Sender<Result<Bytes, std::io::Error>>) {
    let page = limits.page.max(1);
    let mut after = None;
    let mut exported = 0u64;
    loop {
        let rows = tokio::select! {
            () = tx.closed() => return,
            rows = fetch(after, page) => rows,
        };
        let rows = match rows {
            Ok(rows) => rows,
            Err(e) => {
                // The content of the error is the database's; the client only learns that the stream broke.
                tracing::warn!(error = %e, "the audit export broke off");
                let _ = tx.send(Err(std::io::Error::other("the audit export broke off"))).await;
                return;
            }
        };
        let more_in_log = rows.len() >= page as usize;
        let room = limits.max_rows.saturating_sub(exported);
        let take = usize::try_from(room).unwrap_or(usize::MAX).min(rows.len());
        let mut chunk = Vec::new();
        for event in rows.iter().take(take) {
            // Serializing these plain structs cannot fail; if it did, the stream breaks like any other error.
            let Ok(line) = serde_json::to_vec(&wire(event.clone(), None)) else {
                let _ = tx.send(Err(std::io::Error::other("the audit export broke off"))).await;
                return;
            };
            chunk.extend_from_slice(&line);
            chunk.push(b'\n');
        }
        exported = exported.saturating_add(take as u64);
        let cut = take < rows.len();
        if let Some(last) = take.checked_sub(1).and_then(|i| rows.get(i)) {
            after = Some(last.cursor());
        }
        // At the limit with a full page behind it there may be more, which one more row tells.
        let at_limit = !cut && exported >= limits.max_rows && more_in_log;
        let ended = if cut {
            Some(false)
        } else if !more_in_log {
            Some(true)
        } else if at_limit {
            let probe = tokio::select! {
                () = tx.closed() => return,
                probe = fetch(after, 1) => probe,
            };
            match probe {
                Ok(rows) => Some(rows.is_empty()),
                Err(e) => {
                    tracing::warn!(error = %e, "the audit export broke off");
                    let _ = tx.send(Err(std::io::Error::other("the audit export broke off"))).await;
                    return;
                }
            }
        } else {
            None
        };
        if let Some(complete) = ended {
            let end = AuditExportEnd { complete: complete.then_some(true), truncated: (!complete).then_some(true), exported };
            if let Ok(line) = serde_json::to_vec(&end) {
                chunk.extend_from_slice(&line);
                chunk.push(b'\n');
            }
        }
        if !chunk.is_empty() && tx.send(Ok(Bytes::from(chunk))).await.is_err() {
            return;
        }
        if ended.is_some() {
            return;
        }
    }
}

/// `GET /api/v1/admin/audit/export?user&action&before_ms`: the log as NDJSON (`before_ms` also limits it, besides the fixed end).
async fn admin_export(State(state): State<Shared>, params: Result<Query<Params>, QueryRejection>) -> Result<Response, ApiError> {
    let Query(params) = params?;
    let (user, action) = filters(&params)?;
    if params.before_ms.is_some_and(|before| before < 0) {
        return Err(ApiError::invalid("before_ms is a time in milliseconds"));
    }
    let before_ms = params.before_ms;
    let limits = state.audit_export;
    // The end of the log is fixed now: events that arrive while the export runs are not in it.
    let until_ms = state.now_ms();
    let (tx, rx) = mpsc::channel(limits.channel.max(1));
    let reader = state.clone();
    let fetch: Fetch<'static> = Box::new(move |after, rows| {
        let meta = reader.meta.clone();
        Box::pin(async move { meta.audit_list(&AuditQuery { user, action, before_ms, until_ms: Some(until_ms), after, limit: rows }).await })
    });
    tokio::spawn(export_pages(fetch, limits, tx));
    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header::HeaderValue::from_static("application/x-ndjson"));
    headers.insert(header::CONTENT_DISPOSITION, header::HeaderValue::from_static(EXPORT_DISPOSITION));
    headers.insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    Ok(response)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use craft_cloud_meta::{AuditAction, AuditOutcome};

    use super::*;

    fn event(n: i64) -> AuditEvent {
        AuditEvent {
            id: Uuid::from_u128(n as u128),
            at_ms: 1000 - n,
            actor_user: None,
            actor_device: None,
            action: AuditAction::AuthLogin,
            target_kind: String::new(),
            target_id: None,
            outcome: AuditOutcome::Success,
            details: "{}".to_owned(),
            count: 1,
        }
    }

    /// A log of `total` events, newest first, read `rows` at a time; `fail_at` makes that call (counting from 0) fail.
    fn fake_log(total: i64, calls: Arc<AtomicUsize>, fail_at: Option<usize>) -> Fetch<'static> {
        Box::new(move |after, rows| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if fail_at == Some(call) {
                    return Err(MetaError::Corrupt("a test failure"));
                }
                let start = after.map_or(0, |c| 1000 - c.at_ms);
                Ok((start + 1..=total).take(rows as usize).map(event).collect())
            })
        })
    }

    async fn run(fetch: Fetch<'static>, limits: AuditExportLimits) -> (Vec<String>, bool) {
        let (tx, mut rx) = mpsc::channel(limits.channel);
        tokio::spawn(export_pages(fetch, limits, tx));
        let mut lines = Vec::new();
        let mut broke = false;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(bytes) => lines.extend(String::from_utf8(bytes.to_vec()).unwrap().lines().map(str::to_owned)),
                Err(_) => broke = true,
            }
        }
        (lines, broke)
    }

    fn limits(page: u32, max_rows: u64) -> AuditExportLimits {
        AuditExportLimits { page, channel: 1, max_rows }
    }

    #[tokio::test]
    async fn an_export_ends_with_complete_when_it_holds_every_event() {
        for total in [0, 1, 4, 5, 6, 12] {
            let (lines, broke) = run(fake_log(total, Arc::default(), None), limits(5, 100)).await;
            assert!(!broke);
            assert_eq!(lines.len() as i64, total + 1, "total {total}");
            assert_eq!(lines.last().unwrap(), &format!("{{\"complete\":true,\"exported\":{total}}}"));
            for line in &lines[..lines.len() - 1] {
                serde_json::from_str::<craft_cloud_proto::api::AuditEvent>(line).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn an_export_over_the_limit_ends_with_truncated_and_counts_only_events() {
        // 12 events, at most 7 lines of events: truncated, in the middle of a page.
        let (lines, broke) = run(fake_log(12, Arc::default(), None), limits(5, 7)).await;
        assert!(!broke);
        assert_eq!(lines.len(), 8);
        assert_eq!(lines.last().unwrap(), r#"{"truncated":true,"exported":7}"#);
        // Exactly at the limit with events behind it: truncated, found by the probe after a full page.
        let (lines, _) = run(fake_log(12, Arc::default(), None), limits(5, 10)).await;
        assert_eq!(lines.len(), 11);
        assert_eq!(lines.last().unwrap(), r#"{"truncated":true,"exported":10}"#);
        // Exactly the limit and nothing behind it: complete, not truncated (a full page, then the probe finds nothing).
        let (lines, _) = run(fake_log(10, Arc::default(), None), limits(5, 10)).await;
        assert_eq!(lines.last().unwrap(), r#"{"complete":true,"exported":10}"#);
        // The limit is a multiple of nothing: a page ending short of it.
        let (lines, _) = run(fake_log(7, Arc::default(), None), limits(5, 7)).await;
        assert_eq!(lines.last().unwrap(), r#"{"complete":true,"exported":7}"#);
    }

    #[tokio::test]
    async fn a_database_error_in_the_middle_ends_the_stream_without_a_last_line() {
        let (lines, broke) = run(fake_log(20, Arc::default(), Some(2)), limits(5, 100)).await;
        assert!(broke);
        assert_eq!(lines.len(), 10, "the two pages before the error arrived");
        assert!(lines.iter().all(|line| !line.contains("complete") && !line.contains("truncated")));
    }

    #[tokio::test]
    async fn a_client_that_goes_away_stops_the_reading() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::channel(1);
        let task = tokio::spawn(export_pages(fake_log(1_000_000, calls.clone(), None), limits(5, u64::MAX), tx));
        // The first page is read, and the client leaves.
        assert!(rx.recv().await.is_some());
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(10), task).await.unwrap().unwrap();
        let after_leaving = calls.load(Ordering::SeqCst);
        // Reads in flight when it left may finish, the channel's depth at most; nothing goes on after that.
        assert!(after_leaving <= 4, "{after_leaving}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(calls.load(Ordering::SeqCst), after_leaving);
    }

    #[test]
    fn cursors_are_opaque_and_checked() {
        let cursor = AuditCursor { at_ms: 12, id: Uuid::now_v7() };
        assert_eq!(decode_cursor(&encode_cursor(cursor)).unwrap(), cursor);
        for bad in ["", "!", "AAAA", &URL_SAFE_NO_PAD.encode([0u8; 23]), &URL_SAFE_NO_PAD.encode([0xffu8; 24]), &URL_SAFE_NO_PAD.encode([0u8; 25])] {
            assert!(decode_cursor(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_limit_is_one_to_the_most_of_the_route() {
        assert_eq!(page_limit(None, 100).unwrap(), 50);
        assert_eq!(page_limit(Some(100), 100).unwrap(), 100);
        assert!(page_limit(Some(101), 100).is_err());
        assert!(page_limit(Some(0), 100).is_err());
    }
}
