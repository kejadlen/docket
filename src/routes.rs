use std::sync::Arc;

use axum::extract::{FromRequestParts, OriginalUri, Path, Query, State as Extract};
use axum::http::header::{CONTENT_TYPE, HOST, ORIGIN};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::Markup;
use serde::Deserialize;
use tower_http::trace::TraceLayer;

use crate::Error;
use crate::lists::{self, View};
use crate::model::{MessageId, State, ThreadId, User};
use crate::store::{Change, Store};
use crate::views::{self, Page, ThreadView};

// caddy-tailscale sets both from the tailnet identity, overwriting whatever
// the client sent.
pub(crate) const IDENTITY_HEADER: &str = "Remote-User";
pub(crate) const SLUG_HEADER: &str = "X-User-Slug";

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
}

impl AppState {
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(store),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/", get(for_me))
        .route("/{lane}", get(lane))
        .route("/search", get(search))
        .route("/messages/{id}/state", post(set_state))
        .route("/messages/{id}/folder", post(set_folder))
        .route("/messages/{id}/assignees", post(toggle_assignee))
        .route("/threads/{id}/comments", post(add_comment))
        .route("/undo", post(undo))
        .route("/assets/gloss.css", get(gloss_css))
        .route("/assets/docket.css", get(docket_css))
        .route("/assets/alpine.js", get(alpine_js))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// The requesting user, from the identity headers caddy-tailscale adds. Anyone
/// Tailscale lets through is a user; the first request adds them. Missing
/// either header means the proxy isn't doing its job, so nobody gets in.
struct Me(User);

impl FromRequestParts<AppState> for Me {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Error> {
        // Tailscale identifies every request from this machine, so a page on
        // another site could post forms as us; browsers send Origin on those.
        if parts.method == Method::POST && !same_origin(&parts.headers) {
            return Err(Error::Forbidden("cross-site request"));
        }
        let present = |name| header(&parts.headers, name).filter(|v| !v.trim().is_empty());
        let (Some(login), Some(slug)) = (present(IDENTITY_HEADER), present(SLUG_HEADER)) else {
            return Err(Error::Unauthenticated);
        };
        Ok(Me(state.store.sign_in(login, slug)?))
    }
}

pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

pub(crate) fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = header(headers, ORIGIN.as_str()) else {
        return true;
    };
    let origin_host = origin.split_once("://").map(|(_, host)| host);
    origin_host.is_some() && origin_host == header(headers, HOST.as_str())
}

#[derive(Deserialize)]
struct Selection {
    m: Option<MessageId>,
    q: Option<String>,
}

async fn for_me(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    OriginalUri(uri): OriginalUri,
    Query(sel): Query<Selection>,
) -> Result<Markup, Error> {
    render(&state, &me, &View::ForMe, sel.m, &uri.to_string())
}

async fn lane(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    OriginalUri(uri): OriginalUri,
    Path(lane): Path<String>,
    Query(sel): Query<Selection>,
) -> Result<Markup, Error> {
    let lane = State::from_slug(&lane)
        .filter(|s| State::LANES.contains(s))
        .ok_or(Error::NotFound("lane"))?;
    render(&state, &me, &View::Lane(lane), sel.m, &uri.to_string())
}

async fn search(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    OriginalUri(uri): OriginalUri,
    Query(sel): Query<Selection>,
) -> Result<Markup, Error> {
    let view = View::Search(sel.q.unwrap_or_default());
    render(&state, &me, &view, sel.m, &uri.to_string())
}

fn render(
    state: &AppState,
    me: &User,
    view: &View,
    selected: Option<MessageId>,
    here: &str,
) -> Result<Markup, Error> {
    let store = &state.store;
    let thread = selected.map(|id| open_thread(store, me, id)).transpose()?;
    let nav = std::iter::once(View::ForMe)
        .chain(State::LANES.map(View::Lane))
        .map(|v| lists::count(store, me, &v).map(|n| (v, n)))
        .collect::<Result<_, _>>()?;
    Ok(views::page(&Page {
        me,
        view,
        now: store.now(),
        users: store.users()?,
        folders: store.folders()?,
        nav,
        sections: lists::sections(store, me, view)?,
        unread: store.unread(&me.login)?,
        selected,
        thread,
        flash: store.take_flash(&me.login),
        here,
    }))
}

/// Opening a thread reads the selected message and the latest one, the two
/// that open expanded.
fn open_thread(store: &Store, me: &User, id: MessageId) -> Result<ThreadView, Error> {
    let msg = store.message(id)?.ok_or(Error::NotFound("message"))?;
    let latest = store
        .thread_messages(msg.thread)?
        .last()
        .map_or(id, |m| m.id);
    store.mark_read(&me.login, id)?;
    store.mark_read(&me.login, latest)?;
    Ok(ThreadView {
        thread: store.thread(msg.thread)?.ok_or(Error::NotFound("thread"))?,
        account: store
            .thread_account(msg.thread)?
            .ok_or(Error::NotFound("account"))?,
        timeline: store.timeline(msg.thread)?,
        latest,
    })
}

/// Only same-site paths, so a form can't bounce the browser elsewhere.
pub(crate) fn safe_back(back: &str) -> &str {
    if back.starts_with('/') && !back.starts_with("//") && !back.contains('\\') {
        back
    } else {
        "/"
    }
}

#[derive(Deserialize)]
struct StateForm {
    state: String,
    back: String,
}

async fn set_state(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    Path(id): Path<MessageId>,
    Form(form): Form<StateForm>,
) -> Result<Redirect, Error> {
    let to = State::from_slug(&form.state).ok_or(Error::BadRequest("unknown state"))?;
    edit(&state, &me, id, Change::State(to), &form.back).await
}

#[derive(Deserialize)]
struct FolderForm {
    folder: String,
    back: String,
}

async fn set_folder(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    Path(id): Path<MessageId>,
    Form(form): Form<FolderForm>,
) -> Result<Redirect, Error> {
    let folder = Some(form.folder).filter(|f| !f.is_empty());
    edit(&state, &me, id, Change::Folder(folder), &form.back).await
}

#[derive(Deserialize)]
pub(crate) struct UserForm {
    pub(crate) user: String,
    pub(crate) back: String,
}

async fn toggle_assignee(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    Path(id): Path<MessageId>,
    Form(form): Form<UserForm>,
) -> Result<Redirect, Error> {
    edit(
        &state,
        &me,
        id,
        Change::ToggleAssignee(form.user),
        &form.back,
    )
    .await
}

async fn edit(
    state: &AppState,
    me: &User,
    id: MessageId,
    change: Change,
    back: &str,
) -> Result<Redirect, Error> {
    state.store.edit(&me.login, id, change)?;
    Ok(Redirect::to(safe_back(back)))
}

#[derive(Deserialize)]
struct CommentForm {
    text: String,
    back: String,
}

async fn add_comment(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    Path(thread): Path<ThreadId>,
    Form(form): Form<CommentForm>,
) -> Result<Redirect, Error> {
    state.store.add_comment(&me.login, thread, &form.text)?;
    Ok(Redirect::to(safe_back(&form.back)))
}

#[derive(Deserialize)]
struct BackForm {
    back: String,
}

async fn undo(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    Form(form): Form<BackForm>,
) -> Result<Redirect, Error> {
    state.store.undo(&me.login)?;
    Ok(Redirect::to(safe_back(&form.back)))
}

async fn gloss_css() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/gloss.css"),
    )
}

async fn docket_css() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/docket.css"),
    )
}

async fn alpine_js() -> impl IntoResponse {
    (
        [(CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/vendor/alpine-3.17.4.min.js"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_stays_on_site() {
        assert_eq!(safe_back("/do?m=4"), "/do?m=4");
        assert_eq!(safe_back("//evil.example"), "/");
        assert_eq!(safe_back("https://evil.example"), "/");
        assert_eq!(safe_back("/\\evil.example"), "/");
    }
}
