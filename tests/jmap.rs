//! The per-credential session flow against a stub Fastmail: each token
//! opens a session, its mail imports, and the accounts land in the store
//! with the rights the server reported.

use std::net::SocketAddr;

use axum::http::header::{AUTHORIZATION, HOST};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use docket::jmap::{CORE, Client, Credential, MAIL, SUBMISSION};
use docket::model::{Kind, State};
use docket::store::{Clock, Filter, Store};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const HOUSEHOLD_TOKEN: &str = "household-token";
const ELI_TOKEN: &str = "eli-token";
const BARE_TOKEN: &str = "bare-token";
const HOUSEHOLD: &str = "household@example.com";
const ELI: &str = "eli@example.com";
const BARE: &str = "bare@example.com";

/// Fires the tracing events so their fields are real (sync logs land in
/// the test output).
fn init_tracing() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_test_writer()
            .init();
    });
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(AUTHORIZATION)?.to_str().ok()
}

/// The account a token maps to, or None when nobody holds it.
fn account_of(authorization: &str) -> Option<&'static str> {
    match authorization {
        value if value == format!("Bearer {HOUSEHOLD_TOKEN}") => Some(HOUSEHOLD),
        value if value == format!("Bearer {ELI_TOKEN}") => Some(ELI),
        value if value == format!("Bearer {BARE_TOKEN}") => Some(BARE),
        _ => None,
    }
}

/// The session the token's scopes would earn: the household token carries
/// mail and submission, Eli's is read-only mail only (the spike's shapes).
fn session_document(headers: &HeaderMap, authorization: &str) -> Option<Value> {
    let account = account_of(authorization)?;
    let name = match account {
        HOUSEHOLD => "Household",
        ELI => "Eli",
        _ => "Bare",
    };
    let host = headers
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("stub");
    let mut capabilities = json!({ CORE: {}, MAIL: {} });
    let mut primary = json!({ MAIL: account });
    if account == HOUSEHOLD {
        capabilities[SUBMISSION] = json!({});
        primary[SUBMISSION] = json!(account);
    }
    Some(json!({
        "username": account,
        "apiUrl": format!("http://{host}/api"),
        "accounts": { account: {
            "name": name,
            "isPersonal": true,
            "isReadOnly": account == ELI,
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

/// The mailboxes both accounts carry: system boxes by role, `Docket/`
/// labels, and role-less folders.
fn mailboxes() -> Vec<Value> {
    let rights = json!({
        "mayReadItems": true, "mayAddItems": true, "mayRemoveItems": true,
        "maySetKeywords": true, "mayCreateChild": true, "mayRename": true,
        "mayDelete": true, "maySubmit": true,
    });
    let restricted = json!({
        "mayReadItems": true, "mayAddItems": false, "mayRemoveItems": false,
        "maySetKeywords": false, "mayCreateChild": false, "mayRename": false,
        "mayDelete": false, "maySubmit": false,
    });
    [
        ("MB-in", "Inbox", Some("inbox"), &rights),
        ("MB-sent", "Sent", Some("sent"), &rights),
        ("MB-drafts", "Drafts", Some("drafts"), &rights),
        ("MB-sched", "Scheduled", Some("scheduled"), &restricted),
        ("MB-do", "Docket/Do", None, &rights),
        ("MB-watch", "Docket/Watch", None, &rights),
        ("MB-receipts", "Receipts", None, &rights),
        ("MB-school", "School", None, &rights),
    ]
    .into_iter()
    .map(|(id, name, role, rights)| {
        json!({"id": id, "name": name, "role": role, "myRights": rights})
    })
    .collect()
}

/// One email. `body` is Some for plain text and None for HTML-only mail,
/// which leaves the preview to speak for it.
#[allow(clippy::too_many_arguments)]
fn email_json(
    id: &str,
    thread: &str,
    from: (&str, &str),
    to: &str,
    in_mailboxes: &[&str],
    subject: &str,
    body: Option<&str>,
    preview: &str,
) -> Value {
    let mut mail = json!({
        "id": id,
        "threadId": thread,
        "messageId": format!("{id}@chislan.family"),
        "mailboxIds": mailbox_ids(in_mailboxes),
        "receivedAt": "2026-10-02T12:00:00Z",
        "sentAt": "2026-10-02T11:00:00Z",
        "from": [{"name": from.0, "email": from.1}],
        "to": [{"name": to, "email": format!("{to}@example.com")}],
        "cc": [],
        "bcc": [],
        "subject": subject,
        "preview": preview,
    });
    if let Some(text) = body {
        mail["textBody"] = json!([{"partId": "p1", "type": "text/plain"}]);
        mail["bodyValues"] = json!({"p1": {"value": text}});
    }
    mail
}

/// Builds the mailboxIds membership map.
fn mailbox_ids(ids: &[&str]) -> Value {
    Value::Object(ids.iter().map(|id| (id.to_string(), json!(true))).collect())
}

/// Each account's mail. The household Inbox holds a plain message, a
/// `Docket/Do`-labeled one, and a filed-and-labeled one; the Sent mailbox
/// holds one attributable reply and one shared-identity reply, only the
/// first of which belongs to an Inbox thread.
fn emails_of(account: &str) -> Vec<Value> {
    match account {
        HOUSEHOLD => vec![
            email_json(
                "E-roofer1",
                "T-roofer",
                ("Northwind Roofing", "office@northwind.co"),
                "household",
                &["MB-in"],
                "Gutter repair estimate",
                Some("Estimate attached: $1,840."),
                "Estimate attached",
            ),
            email_json(
                "E-roofer2",
                "T-roofer",
                ("Northwind Roofing", "office@northwind.co"),
                "household",
                &["MB-in", "MB-do"],
                "Gutter repair estimate",
                Some("Revised estimate: $2,120."),
                "Revised estimate",
            ),
            email_json(
                "E-school",
                "T-school",
                ("Lincoln High School", "office@lincolnhigh.org"),
                "household",
                &["MB-in", "MB-watch", "MB-school"],
                "Field trip",
                None,
                "Buses now return at 4:15.",
            ),
            email_json(
                "E-reply1",
                "T-roofer",
                ("Sam", "sam@example.com"),
                "office@northwind.co",
                &["MB-sent"],
                "Re: Gutter repair estimate",
                Some("Does this include the downspout?"),
                "Does this include",
            ),
            email_json(
                "E-reply2",
                "T-school",
                ("Household", HOUSEHOLD),
                "office@lincolnhigh.org",
                &["MB-sent"],
                "Re: Field trip",
                Some("Eli has pickup that day."),
                "Eli has pickup",
            ),
            email_json(
                "E-reply3",
                "T-ancient",
                ("Sam", "sam@example.com"),
                "office@northwind.co",
                &["MB-sent"],
                "Re: gutters (2024)",
                Some("Thanks, we went with someone else."),
                "Thanks",
            ),
            // A senderless oddity in the Inbox: imported as nothing.
            {
                let mut noise = email_json(
                    "E-noise",
                    "T-noise",
                    ("", ""),
                    "household",
                    &["MB-in"],
                    "(no subject)",
                    None,
                    "",
                );
                noise["from"] = json!([]);
                noise
            },
            // A draft in an Inbox thread: fetched with the thread, kept
            // out as not sent.
            email_json(
                "E-draft",
                "T-school",
                ("Sam", "sam@example.com"),
                "office@lincolnhigh.org",
                &["MB-drafts"],
                "Re: Field trip (draft)",
                Some("Half-written reply."),
                "Half-written",
            ),
        ],
        ELI => vec![email_json(
            "E-practice",
            "T-practice",
            ("Coach Diaz", "coach@lincolnhigh.org"),
            "eli",
            &["MB-in"],
            "Practice moved",
            Some("Practice moves to 5pm this week."),
            "Practice moves",
        )],
        _ => Vec::new(),
    }
}

/// Each account's threads and every email in them.
fn threads_of(account: &str) -> Vec<Value> {
    match account {
        HOUSEHOLD => vec![
            json!({"id": "T-roofer", "emails": ["E-roofer1", "E-roofer2", "E-reply1"]}),
            json!({"id": "T-school", "emails": ["E-school", "E-reply2", "E-draft"]}),
            json!({"id": "T-ancient", "emails": ["E-reply3"]}),
        ],
        ELI => vec![json!({"id": "T-practice", "emails": ["E-practice"]})],
        _ => Vec::new(),
    }
}

async fn api(headers: HeaderMap, body: String) -> Response {
    let Some(account) = bearer(&headers).and_then(account_of) else {
        return (StatusCode::UNAUTHORIZED, "unknown token").into_response();
    };
    // The request is Docket's own; a surprise here is a bug in this test.
    let request: Value = serde_json::from_str(&body).expect("a json request");
    let call = &request["methodCalls"][0];
    let method = call[0].as_str().expect("a method name").to_owned();
    let args = &call[1];

    let reply = match method.as_str() {
        "Mailbox/get" => json!({"accountId": account, "state": "s", "notFound": [],
                                "list": mailbox_list(account)}),
        "Email/query" => {
            let inbox = args["filter"]["inMailbox"].as_str().expect("an inMailbox");
            let position = args["position"].as_u64().unwrap_or_default() as usize;
            let limit = args["limit"].as_u64().unwrap_or_default() as usize;
            let inbox_emails = emails_of(account);
            let ids: Vec<&str> = inbox_emails
                .iter()
                .filter(|e| e["mailboxIds"][inbox] == json!(true))
                .filter_map(|e| e["id"].as_str())
                .collect();
            json!({"accountId": account, "queryState": "s",
                   "ids": ids.iter().skip(position).take(limit).copied().collect::<Vec<_>>(),
                   "total": ids.len()})
        }
        "Email/get" => {
            let wanted: Vec<&str> = args["ids"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({"accountId": account, "state": "s", "notFound": [],
                   "list": emails_of(account).into_iter()
                       .filter(|e| wanted.contains(&e["id"].as_str().expect("an id")))
                       .collect::<Vec<_>>()})
        }
        "Thread/get" => {
            let wanted: Vec<&str> = args["ids"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({"accountId": account, "state": "s", "notFound": [],
                   "list": threads_of(account).into_iter()
                       .filter(|t| wanted.contains(&t["id"].as_str().expect("an id")))
                       .collect::<Vec<_>>()})
        }
        other => panic!("unexpected method {other}"),
    };
    Json(json!({"methodResponses": [[method, reply, "0"]]})).into_response()
}

/// The BARE account's session lists every mailbox except Inbox — an
/// account Docket can see but has nothing to triage.
fn mailbox_list(account: &str) -> Vec<Value> {
    if account == BARE {
        mailboxes()
            .into_iter()
            .filter(|m| m["id"] != json!("MB-in"))
            .collect()
    } else {
        mailboxes()
    }
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
async fn each_credential_opens_a_session_and_imports_its_mail() {
    init_tracing();
    let addr = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let household = credential(&dir, "household", HOUSEHOLD_TOKEN);
    let eli = credential(&dir, "eli", ELI_TOKEN);
    let bare = credential(&dir, "bare", BARE_TOKEN);

    let client = Client::new(format!("http://{addr}/session")).unwrap();
    let store = Store::open_in_memory(Clock::System).unwrap();
    // Sent-mail attribution matches From addresses against users.
    store.sign_in("sam@example.com", "Sam").unwrap();
    for credential in [&household, &eli] {
        client.sync_account(credential, &store).await.unwrap();
    }

    let accounts = store.accounts().unwrap();
    let slugs: Vec<_> = accounts.iter().map(|a| a.slug.as_str()).collect();
    assert_eq!(slugs, ["household", "eli"]);
    let household_row = accounts.first().unwrap();
    assert!(!household_row.read_only);
    assert_eq!(household_row.address, HOUSEHOLD);
    assert!(accounts.last().unwrap().read_only);

    // Folders come from the role-less mailboxes outside Docket/.
    assert_eq!(store.folders().unwrap(), ["Receipts", "School"]);

    let mut all = store.messages(Filter::Search("")).unwrap();
    all.sort_by_key(|m| m.message_id.clone());
    assert_eq!(all.len(), 6, "{all:#?}");

    let by_id = |id: &str| {
        all.iter()
            .find(|m| m.message_id == format!("{id}@chislan.family"))
            .unwrap_or_else(|| panic!("missing {id}"))
    };

    // Inbox mail lands Inbox and unassigned, labeled mail adopts its
    // label, and a filed message carries its folder.
    let roofer1 = by_id("E-roofer1");
    assert_eq!(roofer1.values().unwrap().state, State::Inbox);
    assert_eq!(roofer1.values().unwrap().folder, None);
    assert!(roofer1.values().unwrap().assignees.is_empty());
    assert_eq!(roofer1.body, "Estimate attached: $1,840.");
    assert_eq!(by_id("E-roofer2").values().unwrap().state, State::Do);
    let school = by_id("E-school");
    assert_eq!(school.values().unwrap().state, State::Watch);
    assert_eq!(school.values().unwrap().folder.as_deref(), Some("School"));
    // HTML-only mail keeps the preview as its body.
    assert_eq!(school.body, "Buses now return at 4:15.");
    let practice = by_id("E-practice");
    assert_eq!(practice.values().unwrap().state, State::Inbox);

    // Sent replies join their threads, attributed by From when a login
    // matches, and the shared identity stays unattributed. A sent
    // message in a thread the Inbox never pulled stays out.
    let reply1 = by_id("E-reply1");
    assert_eq!(reply1.thread, roofer1.thread);
    assert!(
        matches!(&reply1.kind, Kind::Sent { by, .. } if by.as_deref() == Some("sam@example.com"))
    );
    assert!(reply1.values().is_none());
    let shared = by_id("E-reply2");
    assert_eq!(shared.thread, school.thread);
    assert!(matches!(&shared.kind, Kind::Sent { by, .. } if by.is_none()));
    assert!(
        all.iter()
            .all(|m| m.message_id != "E-reply3@chislan.family")
    );

    // Threads group their messages under one subject.
    assert_eq!(
        store.thread(roofer1.thread).unwrap().unwrap().subject,
        "Gutter repair estimate"
    );
    assert_eq!(
        store.thread(school.thread).unwrap().unwrap().subject,
        "Field trip"
    );
    assert_ne!(roofer1.thread, school.thread);

    // Re-opening a session is the refresh path: the same mail re-reads,
    // not duplicates.
    client.sync_account(&household, &store).await.unwrap();
    assert_eq!(store.messages(Filter::Search("")).unwrap().len(), 6);

    // An account with no Inbox mailbox still records itself; there is
    // just no mail to pull.
    client.sync_account(&bare, &store).await.unwrap();
    assert_eq!(store.accounts().unwrap().len(), 3);
    assert_eq!(store.messages(Filter::Search("")).unwrap().len(), 6);
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
