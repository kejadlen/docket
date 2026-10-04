//! The per-credential session flow against a stub Fastmail: each token
//! opens a session, and the accounts land in the store with the rights
//! the server reported.

use std::net::SocketAddr;

use axum::http::header::{AUTHORIZATION, HOST};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use docket::jmap::{CORE, Client, Credential, MAIL, SUBMISSION};
use docket::store::{Clock, Store};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const HOUSEHOLD_TOKEN: &str = "household-token";
const ELI_TOKEN: &str = "eli-token";

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(AUTHORIZATION)?.to_str().ok()
}

/// The session the token's scopes would earn: the household token carries
/// mail and submission, Eli's is read-only mail only (the spike's shapes).
fn session_document(headers: &HeaderMap, authorization: &str) -> Option<Value> {
    let (id, name, read_only, submission) = match authorization {
        value if value == format!("Bearer {HOUSEHOLD_TOKEN}") => {
            ("household@example.com", "Household", false, true)
        }
        value if value == format!("Bearer {ELI_TOKEN}") => ("eli@example.com", "Eli", true, false),
        _ => return None,
    };
    let host = headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("stub");
    let mut capabilities = json!({ CORE: {}, MAIL: {} });
    let mut primary = json!({ MAIL: id });
    if submission {
        capabilities[SUBMISSION] = json!({});
        primary[SUBMISSION] = json!(id);
    }
    Some(json!({
        "username": id,
        "apiUrl": format!("http://{host}/api"),
        "accounts": { id: {
            "name": name,
            "isPersonal": true,
            "isReadOnly": read_only,
            "accountCapabilities": capabilities,
        }},
        "primaryAccounts": primary,
    }))
}

async fn session(headers: HeaderMap) -> Response {
    match bearer(&headers).and_then(|token| session_document(&headers, token)) {
        Some(document) => Json(document).into_response(),
        None => (StatusCode::UNAUTHORIZED, "unknown token").into_response(),
    }
}

async fn api(headers: HeaderMap, body: String) -> Response {
    let authorized =
        bearer(&headers).is_some_and(|token| session_document(&headers, token).is_some());
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "unknown token").into_response();
    }
    // The request is Docket's own; a surprise here is a bug in this test.
    let request: Value = serde_json::from_str(&body).expect("a json request");
    let call = &request["methodCalls"][0];
    assert_eq!(call[0], "Mailbox/get");
    let account_id = call[1]["accountId"].as_str().expect("an accountId");
    Json(json!({
        "methodResponses": [[
            "Mailbox/get",
            {"accountId": account_id, "state": "s", "notFound": [], "list": [
                {"id": "M1", "name": "Inbox", "role": "inbox",
                 "myRights": {"mayReadItems": true, "mayAddItems": true,
                              "mayRemoveItems": true, "maySetKeywords": true,
                              "mayCreateChild": false, "mayRename": false,
                              "mayDelete": false, "maySubmit": true}},
                {"id": "M2", "name": "Receipts", "role": null,
                 "myRights": {"mayReadItems": true, "mayAddItems": true,
                              "mayRemoveItems": true, "maySetKeywords": true,
                              "mayCreateChild": true, "mayRename": true,
                              "mayDelete": true, "maySubmit": true}},
            ]},
            "0",
        ]],
    }))
    .into_response()
}

async fn serve() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/session", get(session))
        .route("/api", post(api));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Writes the token to its file with a trailing newline, the way a secret
/// mounted from systemd or Docker arrives.
fn credential(dir: &tempfile::TempDir, name: &str, token: &str) -> Credential {
    let token_file = dir.path().join(format!("{name}.token"));
    fs_err::write(&token_file, format!("{token}\n")).unwrap();
    Credential {
        name: name.to_owned(),
        token_file: camino::Utf8PathBuf::try_from(token_file).unwrap(),
    }
}

#[tokio::test]
async fn each_credential_opens_a_session_and_derives_its_rights() {
    let addr = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let household = credential(&dir, "household", HOUSEHOLD_TOKEN);
    let eli = credential(&dir, "eli", ELI_TOKEN);

    let client = Client::new(format!("http://{addr}/session")).unwrap();
    let store = Store::open_in_memory(Clock::System).unwrap();
    for credential in [&household, &eli] {
        client.sync_account(credential, &store).await.unwrap();
    }

    let accounts = store.accounts().unwrap();
    let slugs: Vec<_> = accounts.iter().map(|a| a.slug.as_str()).collect();
    assert_eq!(slugs, ["household", "eli"]);
    let household_row = accounts.first().unwrap();
    assert!(!household_row.read_only);
    assert_eq!(household_row.name, "Household");
    assert_eq!(household_row.address, "household@example.com");
    let eli_row = accounts.last().unwrap();
    assert!(eli_row.read_only);
    assert_eq!(eli_row.address, "eli@example.com");

    // Re-opening a session is the refresh path: re-read, not duplicate.
    client.sync_account(&household, &store).await.unwrap();
    assert_eq!(store.accounts().unwrap().len(), 2);
}

#[tokio::test]
async fn a_rejected_token_fails_the_sync() {
    let addr = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let wrong = credential(&dir, "household", "not-the-token");

    let client = Client::new(format!("http://{addr}/session")).unwrap();
    let store = Store::open_in_memory(Clock::System).unwrap();
    let err = client
        .sync_account(&wrong, &store)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("401"), "{err}");
    assert!(store.accounts().unwrap().is_empty());
}
