use std::net::SocketAddr;

use axum::Router;
use docket::fixtures;
use docket::routes::{AppState, router};
use reqwest::{Client, StatusCode, redirect};
use tokio::net::TcpListener;

const SAM: &str = "sam@example.com";
const ALEX: &str = "alex@example.com";
const EVE: &str = "eve@example.com";

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn spawn() -> SocketAddr {
    serve(router(AppState::new(docket::fixtures::store().unwrap()))).await
}

fn client() -> Client {
    Client::builder()
        .redirect(redirect::Policy::none())
        .build()
        .unwrap()
}

/// What the proxy would send as `X-User-Slug`: "sam@example.com" is "Sam".
fn slug(login: &str) -> String {
    let name = login.split('@').next().unwrap_or(login);
    let mut chars = name.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn mid(n: u32) -> String {
    fixtures::id(n)
}

async fn get(addr: SocketAddr, login: &str, path: &str) -> (StatusCode, String) {
    let res = client()
        .get(format!("http://{addr}{path}"))
        .header("Remote-User", login)
        .header("X-User-Slug", slug(login))
        .send()
        .await
        .unwrap();
    (res.status(), res.text().await.unwrap())
}

async fn post(
    addr: SocketAddr,
    login: &str,
    path: &str,
    form: &[(&str, &str)],
) -> reqwest::Response {
    client()
        .post(format!("http://{addr}{path}"))
        .header("Remote-User", login)
        .header("X-User-Slug", slug(login))
        .form(form)
        .send()
        .await
        .unwrap()
}

fn location(res: &reqwest::Response) -> &str {
    res.headers()["location"].to_str().unwrap()
}

#[tokio::test]
async fn health_returns_ok() {
    let addr = spawn().await;
    let res = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn requires_both_identity_headers() {
    let addr = spawn().await;
    let res = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    for (login, slug) in [
        (Some(EVE), None),
        (None, Some("Eve")),
        (Some(""), Some("Eve")),
        (Some(EVE), Some(" ")),
    ] {
        let mut req = client().get(format!("http://{addr}/"));
        if let Some(login) = login {
            req = req.header("Remote-User", login);
        }
        if let Some(slug) = slug {
            req = req.header("X-User-Slug", slug);
        }
        let res = req.send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{login:?} {slug:?}");
    }
    // Nobody got added along the way.
    let (_, body) = get(addr, ALEX, &format!("/?m={}", mid(4))).await;
    assert!(!body.contains(EVE));
}

#[tokio::test]
async fn anyone_tailscale_lets_in_is_a_user() {
    let addr = spawn().await;
    let (status, body) = get(addr, "eve@example.com", "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"<span class="who-name">Eve</span>"#));

    // The name follows the latest X-User-Slug.
    let body = client()
        .get(format!("http://{addr}/?m={}", mid(4)))
        .header("Remote-User", "eve@example.com")
        .header("X-User-Slug", "Evie")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(r#"<span class="who-name">Evie</span>"#));
    // Once in, they can be assigned like anyone else.
    assert!(body.contains(r#"value="eve@example.com"><span>Evie</span>"#));
}

#[tokio::test]
async fn for_me_lists_messages_grouped_by_state_and_thread() {
    let addr = spawn().await;
    let (status, body) = get(addr, SAM, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>For me · Docket</title>"));
    assert!(body.contains("Gutter repair estimate"));
    assert!(body.contains("No thread open."));
    // Rows carry sender, age and snippet; values live in the thread.
    assert!(body.contains(r#"<span class="from">Northwind Roofing</span>"#));
    assert!(!body.contains("FOLDERS"));
    // The page names the build.
    let version = format!(
        r#"<span class="type-label version">{}</span>"#,
        docket::VERSION
    );
    assert!(body.contains(&version));
}

#[tokio::test]
async fn lanes_and_search() {
    let addr = spawn().await;
    for (path, needle) in [
        ("/inbox", "Service interruption Oct 4"),
        ("/do", "Exemption renewal"),
        ("/wait", "Claim 4471"),
        ("/watch", "Shipped: furnace filters"),
        ("/search?q=downspout", "Sam → Northwind Roofing"),
    ] {
        let (status, body) = get(addr, SAM, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(body.contains(needle), "{path} should mention {needle}");
    }
    let (_, body) = get(addr, SAM, "/search?q=zzz").await;
    assert!(body.contains("Nothing here."));
    let (_, body) = get(addr, SAM, "/search").await;
    assert!(body.contains("Nothing here."));
    // A search result row links back into the search.
    let (_, body) = get(addr, SAM, "/search?q=checkup").await;
    assert!(body.contains(&format!("/search?q=checkup&amp;m={}", mid(15))));

    for path in ["/done", "/nope", "/folders/Medical", "/?m=999"] {
        let (status, _) = get(addr, SAM, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn thread_view_shows_the_chain_with_values_in_the_gutter() {
    let addr = spawn().await;
    // An unselected page renders the plain shell; a selection marks it,
    // which is what the phone layout swaps panes on, and the thread
    // head carries the mobile back link.
    let (_, body) = get(addr, SAM, "/").await;
    assert!(body.contains(r#"<div class="app">"#));
    assert!(!body.contains("Back to the list"));
    let (status, body) = get(addr, SAM, &format!("/?m={}", mid(4))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(r#"<div class="app sel">"#));
    assert!(body.contains(r#"href="/" aria-label="Back to the list""#));
    assert!(body.contains("<h2>Gutter repair estimate</h2>"));
    // Only the selected email floats.
    assert!(body.contains(&format!(r#"class="item card sel" id="m{}""#, mid(4))));
    assert!(body.contains(&format!(r#"class="item card" id="m{}""#, mid(3))));
    assert!(body.contains(r#"<span class="type-label sent">Sent</span>"#));
    // A reply's quoted history folds behind a toggle, out of the body
    // and its one-line preview.
    assert!(body.contains(r#"confirm by the 3rd.</span><button class="quote-btn" type="button""#));
    assert!(body.contains(
        r#"x-show="open &amp;&amp; quoted" x-cloak>On Sat, Sep 26, 2026 at 3:02 PM, Sam wrote:"#
    ));
    // URLs in an open body link out; the sentence's period stays text.
    assert!(body.contains(
        r#"site visit: <a href="https://photos.northwind.example/visit/926" target="_blank" rel="noopener">https://photos.northwind.example/visit/926</a>.</span>"#
    ));
    assert!(body.contains(r#"<span class="badge accent"><span class="mark"></span>Do</span>"#));
    assert!(body.contains(r#"<span class="badge success"><span class="mark"></span>Done</span>"#));
    assert!(body.contains(&format!(
        "https://app.fastmail.com/mail/search:msgid%3A%3C{}%40fixtures.docket.invalid%3E",
        mid(4)
    )));
    assert!(body.contains(r#"<button class="btn primary" type="submit">Comment</button>"#));
    assert!(body.contains("cc Alex · bcc Pat Lee"));
    assert!(body.contains("cc Sam, Alex"));
    assert!(body.contains("→ Northwind Roofing"));
    assert!(body.contains("The Hendersons paid about $1,600"));
    assert!(body.contains(&format!(r#"action="/messages/{}/state""#, mid(4))));
    assert!(body.contains(&format!(r#"action="/messages/{}/folder""#, mid(4))));
    assert!(body.contains(&format!(r#"action="/messages/{}/assignees""#, mid(4))));
    assert!(body.contains(r#"<span class="type-label value unset">Assign</span>"#));
    assert!(body.contains(r#"<span class="type-label value">House</span>"#));
    assert!(body.contains(&format!(r#"value="/?m={}""#, mid(4))));

    let (_, body) = get(addr, ALEX, &format!("/?m={}", mid(6))).await;
    assert!(body.contains(r#"<span class="type-label value">→ ALEX</span>"#));
    assert!(!body.contains(r#"<span class="type-label value">—</span>"#));
    let (_, body) = get(addr, SAM, &format!("/inbox?m={}", mid(8))).await;
    assert!(body.contains(r#"<span class="type-label value">—</span>"#));
    assert!(!body.contains("Read-only"));
}

#[tokio::test]
async fn html_mail_renders_in_a_sandboxed_frame() {
    let addr = spawn().await;
    let bill = mid(fixtures::BILL);
    let (_, body) = get(addr, SAM, &format!("/inbox?m={bill}")).await;
    // The frame may open popups and nothing else; the closed preview
    // still reads the text.
    assert!(body.contains(&format!(
        r#"<iframe class="html-body" name="html-{bill}" src="/messages/{bill}/html" title="Message" sandbox="allow-popups allow-popups-to-escape-sandbox" referrerpolicy="no-referrer" loading="lazy"></iframe>"#
    )));
    assert!(body.contains(&format!(
        r#"href="/messages/{bill}/html?images=1" target="html-{bill}">Show images</a>"#
    )));
    assert!(body.contains("Amount due $142.18 by October 21."));
    // Text-only mail keeps its text body.
    let (_, body) = get(addr, SAM, &format!("/inbox?m={}", mid(fixtures::WATER))).await;
    assert!(!body.contains("<iframe"));

    let res = client()
        .get(format!("http://{addr}/messages/{bill}/html"))
        .header("Remote-User", SAM)
        .header("X-User-Slug", "Sam")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let header = |name: &str| res.headers()[name].to_str().unwrap().to_owned();
    assert_eq!(header("content-type"), "text/html; charset=utf-8");
    assert_eq!(header("x-content-type-options"), "nosniff");
    assert_eq!(header("referrer-policy"), "no-referrer");
    let csp = header("content-security-policy");
    assert!(csp.contains("img-src data:;"), "{csp}");
    assert!(
        csp.contains("sandbox allow-popups allow-popups-to-escape-sandbox"),
        "{csp}"
    );
    let page = res.text().await.unwrap();
    assert!(page.contains("$142.18"), "{page}");
    assert!(!page.contains("<script"), "{page}");
    assert!(page.contains(r#"target="_blank""#), "{page}");

    let res = client()
        .get(format!("http://{addr}/messages/{bill}/html?images=1"))
        .header("Remote-User", SAM)
        .header("X-User-Slug", "Sam")
        .send()
        .await
        .unwrap();
    let csp = res.headers()["content-security-policy"].to_str().unwrap();
    assert!(csp.contains("img-src data: https: http:;"), "{csp}");

    // Text-only and unknown mail have no page; nor does anyone outside.
    let water = mid(fixtures::WATER);
    let (status, _) = get(addr, SAM, &format!("/messages/{water}/html")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(addr, SAM, "/messages/nope/html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let res = reqwest::get(format!("http://{addr}/messages/{bill}/html"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn edits_show_up_in_the_thread_as_events() {
    let addr = spawn().await;
    let path = format!("/messages/{}/state", mid(4));
    post(addr, SAM, &path, &[("state", "do"), ("back", "/")]).await;

    let (_, body) = get(addr, ALEX, &format!("/?m={}", mid(4))).await;
    assert!(
        body.contains(
            r#"<div class="item event"><div class="body"><span class="event-text">Sam moved to Do</span></div>"#
        ),
        "{body}"
    );
}

#[tokio::test]
async fn read_only_accounts_show_folder_as_plain_text() {
    let addr = spawn().await;
    let (_, body) = get(addr, SAM, &format!("/watch?m={}", mid(20))).await;
    assert!(body.contains(r#"<span class="type-label">Read-only</span>"#));
    assert!(body.contains(r#"<span class="type-label value static">—</span>"#));
    assert!(!body.contains(&format!(r#"action="/messages/{}/folder""#, mid(20))));
    assert!(body.contains(&format!(r#"action="/messages/{}/state""#, mid(20))));
    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/folder", mid(20)),
        &[("folder", "School"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn opening_a_message_marks_it_read() {
    let addr = spawn().await;
    let (_, body) = get(addr, ALEX, "/").await;
    assert!(body.contains(&format!(r#"class="row solo unread" href="/?m={}""#, mid(7))));
    get(addr, ALEX, &format!("/?m={}", mid(7))).await;
    let (_, body) = get(addr, ALEX, "/").await;
    assert!(body.contains(&format!(r#"class="row solo" href="/?m={}""#, mid(7))));
    // Read tracking is per person.
    let (_, body) = get(addr, SAM, "/").await;
    assert!(body.contains(&format!(r#"class="row solo unread" href="/?m={}""#, mid(7))));
}

#[tokio::test]
async fn editing_values_with_undo() {
    let addr = spawn().await;
    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/state", mid(4)),
        &[("state", "do"), ("back", &format!("/?m={}", mid(4)))],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&res), &format!("/?m={}", mid(4)));

    let (_, body) = get(addr, SAM, &format!("/?m={}", mid(4))).await;
    assert!(body.contains("Moved to Do"));
    assert!(body.contains(r#"action="/undo""#));
    // The toast shows once.
    let (_, body) = get(addr, SAM, &format!("/?m={}", mid(4))).await;
    assert!(!body.contains("Moved to Do"));

    let res = post(addr, SAM, "/undo", &[("back", &format!("/?m={}", mid(4)))]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let (_, body) = get(addr, SAM, &format!("/?m={}", mid(4))).await;
    assert!(body.contains("Undone"));
    assert!(!body.contains(r#"action="/undo""#));
    let res = post(addr, SAM, "/undo", &[("back", "/")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/folder", mid(4)),
        &[("folder", "Finance"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/folder", mid(4)),
        &[("folder", ""), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let (_, body) = get(addr, SAM, &format!("/?m={}", mid(4))).await;
    assert!(body.contains("Unfiled"));

    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/assignees", mid(4)),
        &[("user", ALEX), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    // Now it's Alex's; Sam's For me no longer lists it, Alex's does.
    let (_, body) = get(addr, ALEX, "/").await;
    assert!(body.contains(&format!(r#"href="/?m={}""#, mid(4))));
    let (_, body) = get(addr, SAM, "/").await;
    assert!(!body.contains(&format!(r#"href="/?m={}""#, mid(4))));

    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/state", mid(4)),
        &[("state", "read"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/state", mid(2)),
        &[("state", "do"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/state", mid(4)),
        &[("state", "do"), ("back", "https://evil.example")],
    )
    .await;
    assert_eq!(location(&res), "/");
}

#[tokio::test]
async fn comments_join_the_thread() {
    let addr = spawn().await;
    let res = post(
        addr,
        ALEX,
        &format!("/threads/{}/comments", mid(1)),
        &[
            ("text", "I'll call them."),
            ("back", &format!("/?m={}", mid(4))),
        ],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let (_, body) = get(addr, SAM, &format!("/?m={}", mid(4))).await;
    assert!(body.contains("I&#39;ll call them.") || body.contains("I'll call them."));
    let res = post(
        addr,
        ALEX,
        &format!("/threads/{}/comments", mid(1)),
        &[("text", " "), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cross_site_posts_are_refused() {
    let addr = spawn().await;
    let send = |origin: &'static str| {
        client()
            .post(format!("http://{addr}/messages/{}/state", mid(4)))
            .header("Remote-User", SAM)
            .header("X-User-Slug", "Sam")
            .header("Origin", origin)
            .form(&[("state", "do"), ("back", "/")])
            .send()
    };
    let res = send("https://evil.example").await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = send("null").await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let same: &'static str = Box::leak(format!("http://{addr}").into_boxed_str());
    let res = send(same).await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn without_the_dev_feature_there_is_no_user_switch() {
    let addr = spawn().await;
    let res = post(addr, SAM, "/dev/user", &[("user", ALEX), ("back", "/")]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[cfg(feature = "dev")]
mod dev {
    use super::*;

    async fn spawn_dev(store: docket::store::Store) -> SocketAddr {
        serve(docket::dev::router(AppState::new(store))).await
    }

    async fn page(addr: SocketAddr, cookie: Option<&str>) -> String {
        let mut req = client()
            .get(format!("http://{addr}/"))
            // Whatever the client claims, the middleware decides.
            .header("Remote-User", "mallory@example.com")
            .header("X-User-Slug", "Mallory");
        if let Some(cookie) = cookie {
            req = req.header("Cookie", cookie);
        }
        req.send().await.unwrap().text().await.unwrap()
    }

    #[tokio::test]
    async fn picks_a_user_from_a_cookie() {
        let addr = spawn_dev(docket::fixtures::store().unwrap()).await;
        let body = page(addr, None).await;
        assert!(body.contains(r#"<span class="who-name">Alex</span>"#));
        assert!(body.contains(r#"action="/dev/user""#));
        assert!(!body.contains("Mallory"));

        let res = client()
            .post(format!("http://{addr}/dev/user"))
            .form(&[("user", SAM), ("back", "/do")])
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::SEE_OTHER);
        assert_eq!(location(&res), "/do");
        let cookie = res.headers()["set-cookie"].to_str().unwrap();
        assert!(cookie.starts_with("docket_dev_user=sam@example.com;"));

        let body = page(addr, Some("theme=dark; docket_dev_user=sam@example.com")).await;
        assert!(body.contains(r#"<span class="who-name">Sam</span>"#));
        // A cookie for someone unknown falls back to the first user.
        let body = page(addr, Some("docket_dev_user=eve@example.com")).await;
        assert!(body.contains(r#"<span class="who-name">Alex</span>"#));

        let res = client()
            .post(format!("http://{addr}/dev/user"))
            .form(&[("user", "eve"), ("back", "/")])
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = client()
            .post(format!("http://{addr}/dev/user"))
            .header("Origin", "https://evil.example")
            .form(&[("user", SAM), ("back", "/")])
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn with_no_users_is_unauthenticated() {
        let store = docket::store::Store::open_in_memory(docket::store::Clock::Fixed(
            docket::fixtures::now(),
        ))
        .unwrap();
        let addr = spawn_dev(store).await;
        let res = client()
            .get(format!("http://{addr}/"))
            .header("Remote-User", SAM)
            .header("X-User-Slug", "Sam")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }
}

#[tokio::test]
async fn serves_assets() {
    let addr = spawn().await;
    for (path, kind, needle) in [
        ("/assets/gloss.css", "text/css", "--gl-accent-teal"),
        ("/assets/docket.css", "text/css", ".app"),
        ("/assets/alpine.js", "text/javascript", "Alpine"),
    ] {
        let res = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(
            res.headers()["content-type"]
                .to_str()
                .unwrap()
                .starts_with(kind)
        );
        assert!(res.text().await.unwrap().contains(needle));
    }
}

#[tokio::test]
async fn trashing_confirms_through_a_dialog_and_marks_done() {
    let addr = spawn().await;
    // The trash affordance is a dialog on writable accounts: open it
    // with showModal, cancel closes without posting, and the form
    // targets the delete route.
    let (_, body) = get(addr, SAM, &format!("/inbox?m={}", mid(8))).await;
    assert!(body.contains(&format!(r#"id="trash-{}""#, mid(8))));
    assert!(body.contains("showModal"));
    assert!(body.contains("Move to Trash?"));
    assert!(body.contains(r#"formmethod="dialog""#));
    assert!(body.contains(&format!(r#"action="/messages/{}/delete""#, mid(8))));

    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/delete", mid(8)),
        &[("back", &format!("/inbox?m={}", mid(8)))],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let (_, body) = get(addr, SAM, &format!("/search?q=142.18&m={}", mid(8))).await;
    assert!(body.contains(r#"<span class="badge success"><span class="mark"></span>Done</span>"#));

    // Read-only accounts get neither the dialog nor the route.
    let (_, body) = get(addr, SAM, &format!("/watch?m={}", mid(20))).await;
    assert!(!body.contains(r#"id="trash-20""#));
    let res = post(
        addr,
        SAM,
        &format!("/messages/{}/delete", mid(20)),
        &[("back", &format!("/watch?m={}", mid(20)))],
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pages_gzip_for_browsers_that_ask() {
    let addr = spawn().await;
    let res = client()
        .get(format!("http://{addr}/"))
        .header("Remote-User", SAM)
        .header("X-User-Slug", slug(SAM))
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-encoding"], "gzip");
}
