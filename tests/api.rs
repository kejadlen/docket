use std::net::SocketAddr;

use docket::routes::{AppState, router};
use reqwest::{Client, StatusCode, redirect};
use tokio::net::TcpListener;

const SAM: &str = "sam@example.com";
const ALEX: &str = "alex@example.com";

async fn spawn_with(dev: bool) -> SocketAddr {
    let app = router(AppState::new(docket::fixtures::store(), dev));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn spawn() -> SocketAddr {
    spawn_with(false).await
}

fn client() -> Client {
    Client::builder()
        .redirect(redirect::Policy::none())
        .build()
        .unwrap()
}

async fn get(addr: SocketAddr, login: &str, path: &str) -> (StatusCode, String) {
    let res = client()
        .get(format!("http://{addr}{path}"))
        .header("Tailscale-User-Login", login)
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
        .header("Tailscale-User-Login", login)
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
async fn requires_a_known_identity() {
    let addr = spawn().await;
    let res = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let (status, _) = get(addr, "eve@example.com", "/").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn for_me_lists_messages_grouped_by_state_and_thread() {
    let addr = spawn().await;
    let (status, body) = get(addr, SAM, "/").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<title>For me · Docket</title>"));
    assert!(body.contains("Gutter repair estimate"));
    assert!(body.contains("2 OF 4"));
    assert!(body.contains("No thread open."));
    // Rows carry sender, age and snippet; values live in the thread.
    assert!(body.contains(r#"<span class="from">Northwind Roofing</span>"#));
    assert!(!body.contains("FOLDERS"));
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
    assert!(body.contains("/search?q=checkup&amp;m=15"));

    for path in ["/done", "/nope", "/folders/Medical", "/?m=999"] {
        let (status, _) = get(addr, SAM, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn thread_view_shows_the_chain_with_values_in_the_gutter() {
    let addr = spawn().await;
    let (status, body) = get(addr, SAM, "/?m=4").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("<h2>Gutter repair estimate</h2>"));
    // Only the selected email floats.
    assert!(body.contains(r#"class="item card sel" id="m4""#));
    assert!(body.contains(r#"class="item card" id="m3""#));
    assert!(body.contains(r#"<span class="type-label">Internal</span>"#));
    assert!(body.contains(r#"<span class="type-label sent">Sent</span>"#));
    assert!(body.contains(r#"<span class="badge accent"><span class="mark"></span>Do</span>"#));
    assert!(body.contains(r#"<span class="badge success"><span class="mark"></span>Done</span>"#));
    assert!(body.contains(
        "https://app.fastmail.com/mail/search:msgid%3A%3C4%40fixtures.docket.invalid%3E"
    ));
    assert!(body.contains(r#"<button class="btn primary" type="submit">Comment</button>"#));
    assert!(body.contains("cc Alex · bcc Pat Lee"));
    assert!(body.contains("cc Sam, Alex"));
    assert!(body.contains("→ Northwind Roofing"));
    assert!(body.contains("The Hendersons paid about $1,600"));
    assert!(body.contains(r#"action="/messages/4/state""#));
    assert!(body.contains(r#"action="/messages/4/folder""#));
    assert!(body.contains(r#"action="/messages/4/assignees""#));
    assert!(body.contains(r#"<span class="type-label value unset">Assign</span>"#));
    assert!(body.contains(r#"<span class="type-label value">House</span>"#));
    assert!(body.contains(r#"value="/?m=4""#));

    let (_, body) = get(addr, ALEX, "/?m=6").await;
    assert!(body.contains(r#"<span class="type-label value">→ ALEX</span>"#));
    assert!(!body.contains(r#"<span class="type-label value">—</span>"#));
    let (_, body) = get(addr, SAM, "/inbox?m=8").await;
    assert!(body.contains(r#"<span class="type-label value">—</span>"#));
    assert!(!body.contains("Read-only"));
}

#[tokio::test]
async fn read_only_accounts_show_folder_as_plain_text() {
    let addr = spawn().await;
    let (_, body) = get(addr, SAM, "/watch?m=20").await;
    assert!(body.contains(r#"<span class="type-label">Read-only</span>"#));
    assert!(body.contains(r#"<span class="type-label value static">—</span>"#));
    assert!(!body.contains(r#"action="/messages/20/folder""#));
    assert!(body.contains(r#"action="/messages/20/state""#));
    let res = post(
        addr,
        SAM,
        "/messages/20/folder",
        &[("folder", "School"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn opening_a_message_marks_it_read() {
    let addr = spawn().await;
    let (_, body) = get(addr, ALEX, "/").await;
    assert!(body.contains(r#"class="row unread" href="/?m=7""#));
    get(addr, ALEX, "/?m=7").await;
    let (_, body) = get(addr, ALEX, "/").await;
    assert!(body.contains(r#"class="row" href="/?m=7""#));
    // Read tracking is per person.
    let (_, body) = get(addr, SAM, "/").await;
    assert!(body.contains(r#"class="row unread" href="/?m=7""#));
}

#[tokio::test]
async fn editing_values_with_undo() {
    let addr = spawn().await;
    let res = post(
        addr,
        SAM,
        "/messages/4/state",
        &[("state", "do"), ("back", "/?m=4")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&res), "/?m=4");

    let (_, body) = get(addr, SAM, "/?m=4").await;
    assert!(body.contains("Moved to Do"));
    assert!(body.contains(r#"action="/undo""#));
    // The toast shows once.
    let (_, body) = get(addr, SAM, "/?m=4").await;
    assert!(!body.contains("Moved to Do"));

    let res = post(addr, SAM, "/undo", &[("back", "/?m=4")]).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let (_, body) = get(addr, SAM, "/?m=4").await;
    assert!(body.contains("Undone"));
    assert!(!body.contains(r#"action="/undo""#));
    let res = post(addr, SAM, "/undo", &[("back", "/")]).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = post(
        addr,
        SAM,
        "/messages/4/folder",
        &[("folder", "Finance"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let res = post(
        addr,
        SAM,
        "/messages/4/folder",
        &[("folder", ""), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let (_, body) = get(addr, SAM, "/?m=4").await;
    assert!(body.contains("Removed from folder"));

    let res = post(
        addr,
        SAM,
        "/messages/4/assignees",
        &[("user", "alex"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    // Now it's Alex's; Sam's For me no longer lists it, Alex's does.
    let (_, body) = get(addr, ALEX, "/").await;
    assert!(body.contains(r#"href="/?m=4""#));
    let (_, body) = get(addr, SAM, "/").await;
    assert!(!body.contains(r#"href="/?m=4""#));

    let res = post(
        addr,
        SAM,
        "/messages/4/state",
        &[("state", "read"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = post(
        addr,
        SAM,
        "/messages/2/state",
        &[("state", "do"), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let res = post(
        addr,
        SAM,
        "/messages/4/state",
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
        "/threads/1/comments",
        &[("text", "I'll call them."), ("back", "/?m=4")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let (_, body) = get(addr, SAM, "/?m=4").await;
    assert!(body.contains("I&#39;ll call them.") || body.contains("I'll call them."));
    let res = post(
        addr,
        ALEX,
        "/threads/1/comments",
        &[("text", " "), ("back", "/")],
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn cross_site_posts_are_refused() {
    let addr = spawn_with(true).await;
    let send = |origin: &'static str, path: &'static str| {
        client()
            .post(format!("http://{addr}{path}"))
            .header("Tailscale-User-Login", SAM)
            .header("Origin", origin)
            .form(&[("state", "do"), ("user", "alex"), ("back", "/")])
            .send()
    };
    let res = send("https://evil.example", "/messages/4/state")
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = send("null", "/messages/4/state").await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let res = send("https://evil.example", "/dev/user").await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    let same: &'static str = Box::leak(format!("http://{addr}").into_boxed_str());
    let res = send(same, "/messages/4/state").await.unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
}

#[tokio::test]
async fn dev_mode_picks_a_user_from_a_cookie() {
    let addr = spawn_with(true).await;
    let anon = client();
    let body = anon
        .get(format!("http://{addr}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(r#"<span class="who-name">Alex</span>"#));
    assert!(body.contains(r#"action="/dev/user""#));

    let res = anon
        .post(format!("http://{addr}/dev/user"))
        .form(&[("user", "sam"), ("back", "/do")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&res), "/do");
    let cookie = res.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.starts_with("docket_dev_user=sam;"));

    let body = anon
        .get(format!("http://{addr}/"))
        .header("Cookie", "theme=dark; docket_dev_user=sam")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains(r#"<span class="who-name">Sam</span>"#));

    let res = anon
        .post(format!("http://{addr}/dev/user"))
        .form(&[("user", "eve"), ("back", "/")])
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // Outside dev mode, the switch doesn't exist.
    let prod = spawn().await;
    let res = post(prod, SAM, "/dev/user", &[("user", "alex"), ("back", "/")]).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dev_mode_with_no_users_is_unauthenticated() {
    let store = docket::store::Store::default();
    let app = router(AppState::new(store, true));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let res = reqwest::get(format!("http://{addr}/")).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
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
