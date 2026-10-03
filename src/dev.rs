//! Local development without Tailscale, compiled in only with the `dev`
//! feature. A middleware plays caddy-tailscale, setting the identity headers
//! for a user picked from the menu, so requests take the same path they do
//! in production.

use axum::extract::{Request, State};
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::post;
use axum::{Form, Router};

use crate::Error;
use crate::routes::{
    self, AppState, IDENTITY_HEADER, SLUG_HEADER, UserForm, header, safe_back, same_origin,
};

const COOKIE_NAME: &str = "docket_dev_user";

/// The app's routes plus the user switch, with requests signed in as the
/// user the switch chose, or the first user before it's been used.
pub fn router(state: AppState) -> Router {
    routes::router(state.clone())
        .merge(
            Router::new()
                .route("/dev/user", post(switch_user))
                .with_state(state.clone()),
        )
        .layer(middleware::from_fn_with_state(state, identify))
}

async fn identify(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, Error> {
    let store = &state.store;
    let chosen = match cookie(req.headers(), COOKIE_NAME) {
        Some(login) => store.user(login)?,
        None => None,
    };
    let user = match chosen {
        Some(user) => Some(user),
        None => store.users()?.into_iter().next(),
    };
    // Like the proxy, never trust what the client sent.
    let headers = req.headers_mut();
    headers.remove(IDENTITY_HEADER);
    headers.remove(SLUG_HEADER);
    // The slug goes along too: signing in without one would rename the user.
    if let Some(user) = user
        && let (Ok(login), Ok(slug)) = (
            HeaderValue::from_str(&user.login),
            HeaderValue::from_str(&user.slug),
        )
    {
        headers.insert(IDENTITY_HEADER, login);
        headers.insert(SLUG_HEADER, slug);
    }
    Ok(next.run(req).await)
}

async fn switch_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<UserForm>,
) -> Result<Response, Error> {
    if !same_origin(&headers) {
        return Err(Error::Forbidden("cross-site request"));
    }
    if state.store.user(&form.user)?.is_none() {
        return Err(Error::NotFound("user"));
    }
    let cookie = format!("{COOKIE_NAME}={}; Path=/; SameSite=Lax", form.user);
    Ok(([(SET_COOKIE, cookie)], Redirect::to(safe_back(&form.back))).into_response())
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    header(headers, COOKIE.as_str())?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}
