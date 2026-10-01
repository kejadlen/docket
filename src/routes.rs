use std::sync::Arc;

use axum::extract::{FromRequestParts, OriginalUri, Path, Query, State as Extract};
use axum::http::header::{CONTENT_TYPE, COOKIE, HOST, ORIGIN, SET_COOKIE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use maud::Markup;
use serde::Deserialize;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;

use crate::Error;
use crate::lists::View;
use crate::model::{MessageId, State, ThreadId, User};
use crate::store::{Change, Store};
use crate::views::{self, Page};

const IDENTITY_HEADER: &str = "Tailscale-User-Login";
const DEV_COOKIE: &str = "docket_dev_user";

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<RwLock<Store>>,
    /// Without Tailscale in front, fall back to a cookie-chosen user.
    pub dev: bool,
}

impl AppState {
    pub fn new(store: Store, dev: bool) -> Self {
        Self {
            store: Arc::new(RwLock::new(store)),
            dev,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/", get(for_me))
        .route("/{lane}", get(lane))
        .route("/folders/{folder}", get(folder))
        .route("/search", get(search))
        .route("/messages/{id}/state", post(set_state))
        .route("/messages/{id}/folder", post(set_folder))
        .route("/messages/{id}/assignees", post(toggle_assignee))
        .route("/threads/{id}/comments", post(add_comment))
        .route("/undo", post(undo))
        .route("/assets/gloss.css", get(gloss_css))
        .route("/assets/docket.css", get(docket_css))
        .route("/assets/alpine.js", get(alpine_js));
    if state.dev {
        router = router.route("/dev/user", post(dev_user));
    }
    router.layer(TraceLayer::new_for_http()).with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

/// The requesting user, from Tailscale's identity header.
struct Me(User);

impl FromRequestParts<AppState> for Me {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Error> {
        // Tailscale identifies every request from this machine, so a page on
        // another site could post forms as us; browsers send Origin on those.
        if parts.method == Method::POST && !same_origin(&parts.headers) {
            return Err(Error::Forbidden("cross-site request"));
        }
        let store = state.store.read().await;
        if let Some(login) = header(&parts.headers, IDENTITY_HEADER) {
            return store
                .user_by_login(login)
                .cloned()
                .map(Me)
                .ok_or(Error::Unauthenticated);
        }
        if !state.dev {
            return Err(Error::Unauthenticated);
        }
        let chosen = cookie(&parts.headers, DEV_COOKIE).and_then(|slug| store.user(slug));
        chosen
            .or_else(|| store.users.first())
            .cloned()
            .map(Me)
            .ok_or(Error::Unauthenticated)
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = header(headers, ORIGIN.as_str()) else {
        return true;
    };
    let origin_host = origin.split_once("://").map(|(_, host)| host);
    origin_host.is_some() && origin_host == header(headers, HOST.as_str())
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    header(headers, COOKIE.as_str())?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
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
    render(&state, &me, &View::ForMe, sel.m, &uri.to_string()).await
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
    render(&state, &me, &View::Lane(lane), sel.m, &uri.to_string()).await
}

async fn folder(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    OriginalUri(uri): OriginalUri,
    Path(folder): Path<String>,
    Query(sel): Query<Selection>,
) -> Result<Markup, Error> {
    if !state.store.read().await.folders.contains(&folder) {
        return Err(Error::NotFound("folder"));
    }
    render(&state, &me, &View::Folder(folder), sel.m, &uri.to_string()).await
}

async fn search(
    Extract(state): Extract<AppState>,
    Me(me): Me,
    OriginalUri(uri): OriginalUri,
    Query(sel): Query<Selection>,
) -> Result<Markup, Error> {
    let view = View::Search(sel.q.unwrap_or_default());
    render(&state, &me, &view, sel.m, &uri.to_string()).await
}

async fn render(
    state: &AppState,
    me: &User,
    view: &View,
    selected: Option<MessageId>,
    here: &str,
) -> Result<Markup, Error> {
    let mut store = state.store.write().await;
    if let Some(id) = selected {
        let msg = store.message(id).ok_or(Error::NotFound("message"))?;
        // Opening a thread reads the selected message and the latest one,
        // the two that open expanded.
        let latest = store.thread_messages(msg.thread).last().map(|m| m.id);
        store.mark_read(&me.slug, id);
        if let Some(latest) = latest {
            store.mark_read(&me.slug, latest);
        }
    }
    let flash = store.take_flash(&me.slug);
    let store = &*store;
    Ok(views::page(&Page {
        store,
        me,
        view,
        selected: selected.and_then(|id| store.message(id)),
        flash,
        here,
        dev: state.dev,
    }))
}

/// Only same-site paths, so a form can't bounce the browser elsewhere.
fn safe_back(back: &str) -> &str {
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
struct UserForm {
    user: String,
    back: String,
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
    state.store.write().await.edit(&me.slug, id, change)?;
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
    state
        .store
        .write()
        .await
        .add_comment(&me.slug, thread, &form.text)?;
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
    state.store.write().await.undo(&me.slug)?;
    Ok(Redirect::to(safe_back(&form.back)))
}

async fn dev_user(
    Extract(state): Extract<AppState>,
    headers: HeaderMap,
    Form(form): Form<UserForm>,
) -> Result<Response, Error> {
    if !same_origin(&headers) {
        return Err(Error::Forbidden("cross-site request"));
    }
    if state.store.read().await.user(&form.user).is_none() {
        return Err(Error::NotFound("user"));
    }
    let cookie = format!("{DEV_COOKIE}={}; Path=/; SameSite=Lax", form.user);
    Ok(([(SET_COOKIE, cookie)], Redirect::to(safe_back(&form.back))).into_response())
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
