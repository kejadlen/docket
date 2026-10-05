//! The per-credential session flow against a stub Fastmail: each token
//! opens a session, its mail imports, and the accounts land in the store
//! with the rights the server reported.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

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

/// The ACLs of a mailbox the token fully controls.
fn full_rights() -> Value {
    json!({
        "mayReadItems": true, "mayAddItems": true, "mayRemoveItems": true,
        "maySetKeywords": true, "mayCreateChild": true, "mayRename": true,
        "mayDelete": true, "maySubmit": true,
    })
}

/// The mailboxes both accounts carry: system boxes by role, `Docket/`
/// labels, and role-less folders.
fn mailboxes() -> Vec<Value> {
    let rights = full_rights();
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

/// The stub's mutable side: each account's mail as JSON, plus a
/// snapshot of each type per state it has handed out, so `/changes`
/// can diff since any state a client holds.
#[derive(Default)]
struct Stub {
    accounts: BTreeMap<&'static str, AccountMail>,
}

#[derive(Default)]
struct AccountMail {
    mailboxes: Vec<Value>,
    emails: Vec<Value>,
    threads: Vec<Value>,
    /// state → each email's hash at that state.
    email_states: Vec<(String, BTreeMap<String, u64>)>,
    mailbox_states: Vec<(String, BTreeMap<String, u64>)>,
}

type World = Arc<Mutex<Stub>>;

impl Stub {
    fn hash(mail: &Value) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        serde_json::to_string(mail).unwrap().hash(&mut hasher);
        hasher.finish()
    }

    /// The fixture mail under its first states.
    fn seed() -> Self {
        let mut stub = Self::default();
        for account in [HOUSEHOLD, ELI, BARE] {
            stub.accounts.insert(
                account,
                AccountMail {
                    mailboxes: mailbox_list(account),
                    emails: emails_of(account),
                    threads: threads_of(account),
                    ..AccountMail::default()
                },
            );
        }
        for account in [HOUSEHOLD, ELI, BARE] {
            stub.snapshot(account);
        }
        stub
    }

    /// Records the current mail under a fresh state per type, the way
    /// a server bumps its Foo/get `state` when anything changes.
    fn snapshot(&mut self, account: &'static str) {
        let mail = self.accounts.get_mut(account).unwrap();
        let emails = mail
            .emails
            .iter()
            .map(|e| (e["id"].as_str().unwrap().to_owned(), Self::hash(e)))
            .collect();
        let n = mail.email_states.len();
        mail.email_states.push((format!("e{n}"), emails));
        let mailboxes = mail
            .mailboxes
            .iter()
            .map(|m| (m["id"].as_str().unwrap().to_owned(), Self::hash(m)))
            .collect();
        let n = mail.mailbox_states.len();
        mail.mailbox_states.push((format!("m{n}"), mailboxes));
    }

    fn add_email(&mut self, account: &'static str, email: Value) {
        self.accounts.get_mut(account).unwrap().emails.push(email);
        self.snapshot(account);
    }

    fn update_email(&mut self, account: &'static str, id: &str, change: impl FnOnce(&mut Value)) {
        let mail = self.accounts.get_mut(account).unwrap();
        let email = mail
            .emails
            .iter_mut()
            .find(|e| e["id"] == json!(id))
            .unwrap();
        change(email);
        self.snapshot(account);
    }

    fn remove_email(&mut self, account: &'static str, id: &str) {
        let mail = self.accounts.get_mut(account).unwrap();
        mail.emails.retain(|e| e["id"] != json!(id));
        self.snapshot(account);
    }

    fn add_mailbox(&mut self, account: &'static str, mailbox: Value) {
        self.accounts
            .get_mut(account)
            .unwrap()
            .mailboxes
            .push(mailbox);
        self.snapshot(account);
    }
}

/// The `/changes` reply for one type: the diff from the named state to
/// the latest, cut on a snapshot boundary when it exceeds
/// `max_changes`, so `hasMoreChanges` pages like a real server. A
/// `since` the stub never handed out is a state it cannot calculate.
fn changes_reply(
    account: &str,
    states: &[(String, BTreeMap<String, u64>)],
    since: &str,
    max_changes: usize,
) -> Result<Value, ()> {
    let start = states
        .iter()
        .position(|(state, _)| state == since)
        .ok_or(())?;
    let mut now = states[start].1.clone();
    let mut new_state = states[start].0.clone();
    let (mut created, mut updated, mut destroyed) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for (state, snapshot) in &states[start + 1..] {
        let mut touched = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        for (id, hash) in snapshot {
            match now.get(id) {
                None => {
                    touched.0.insert(id.clone());
                }
                Some(old) if old != hash => {
                    touched.1.insert(id.clone());
                }
                _ => {}
            }
        }
        for id in now.keys() {
            if !snapshot.contains_key(id) {
                touched.2.insert(id.clone());
            }
        }
        let n = touched.0.len() + touched.1.len() + touched.2.len();
        if created.len() + updated.len() + destroyed.len() + n > max_changes {
            break;
        }
        created.extend(touched.0);
        updated.extend(touched.1);
        destroyed.extend(touched.2);
        now = snapshot.clone();
        new_state = state.clone();
    }
    Ok(json!({
        "accountId": account, "oldState": since, "newState": new_state,
        "hasMoreChanges": new_state != states.last().unwrap().0,
        "created": created.iter().collect::<Vec<_>>(),
        "updated": updated.iter().collect::<Vec<_>>(),
        "destroyed": destroyed.iter().collect::<Vec<_>>(),
    }))
}

async fn api(world: axum::extract::State<World>, headers: HeaderMap, body: String) -> Response {
    let Some(account) = bearer(&headers).and_then(account_of) else {
        return (StatusCode::UNAUTHORIZED, "unknown token").into_response();
    };
    // The request is Docket's own; a surprise here is a bug in this test.
    let request: Value = serde_json::from_str(&body).expect("a json request");
    let call = &request["methodCalls"][0];
    let method = call[0].as_str().expect("a method name").to_owned();
    let args = &call[1];

    let stub = world.lock().unwrap();
    let mail = stub.accounts.get(account).unwrap();
    let reply = match method.as_str() {
        "Mailbox/get" => json!({
            "accountId": account,
            "state": mail.mailbox_states.last().unwrap().0,
            "notFound": [], "list": mail.mailboxes.clone(),
        }),
        "Email/query" => {
            let inbox = args["filter"]["inMailbox"].as_str().expect("an inMailbox");
            let position = args["position"].as_u64().unwrap_or_default() as usize;
            let limit = args["limit"].as_u64().unwrap_or_default() as usize;
            let ids: Vec<&str> = mail
                .emails
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
            json!({"accountId": account, "state": mail.email_states.last().unwrap().0,
                   "notFound": [],
                   "list": mail.emails.iter()
                       .filter(|e| wanted.contains(&e["id"].as_str().expect("an id")))
                       .cloned().collect::<Vec<_>>()})
        }
        "Thread/get" => {
            let wanted: Vec<&str> = args["ids"]
                .as_array()
                .map(|ids| ids.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            json!({"accountId": account, "state": "s", "notFound": [],
                   "list": mail.threads.iter()
                       .filter(|t| wanted.contains(&t["id"].as_str().expect("an id")))
                       .cloned().collect::<Vec<_>>()})
        }
        "Email/changes" | "Mailbox/changes" => {
            let since = args["sinceState"].as_str().expect("a sinceState");
            let max = args["maxChanges"].as_u64().unwrap_or_default() as usize;
            // A state the tests reserve for making the call fail
            // outright, distinct from one the stub cannot calculate.
            if since.ends_with("-fail") {
                return Json(json!({"methodResponses":
                    [["error", {"type": "serverFail", "description": "stub"}, "0"]]}))
                .into_response();
            }
            let states = if method == "Email/changes" {
                &mail.email_states
            } else {
                &mail.mailbox_states
            };
            match changes_reply(account, states, since, max) {
                Ok(reply) => reply,
                Err(()) => {
                    return Json(json!({"methodResponses":
                        [["error", {"type": "cannotCalculateChanges"}, "0"]]}))
                    .into_response();
                }
            }
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

async fn serve() -> (SocketAddr, World) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let world: World = Arc::new(Mutex::new(Stub::seed()));
    let app = Router::new()
        .route("/session", get(session))
        .route("/api", post(api))
        .with_state(world.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, world)
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
    let (addr, _world) = serve().await;
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
    let (addr, _world) = serve().await;
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

/// The imported message with the given Message-ID, if any.
fn the(store: &Store, message_id: &str) -> Option<docket::model::Message> {
    store
        .messages(Filter::Search(""))
        .unwrap()
        .into_iter()
        .find(|m| m.message_id == message_id)
}

/// A household account synced and ready to poll.
async fn synced(
    addr: &SocketAddr,
    store: &Store,
    credential: &Credential,
) -> (Client, docket::jmap::Sync) {
    let client = Client::new(format!("http://{addr}/session")).unwrap();
    let sync = client.sync_account(credential, store).await.unwrap();
    (client, sync)
}

/// Boots a test world: stub, store with Sam signed in, and the
/// household credential synced.
async fn household_ready() -> (
    World,
    Client,
    Credential,
    docket::jmap::Sync,
    Store,
    tempfile::TempDir,
) {
    init_tracing();
    let (addr, world) = serve().await;
    let dir = tempfile::tempdir().unwrap();
    let household = credential(&dir, "household", HOUSEHOLD_TOKEN);
    let store = Store::open_in_memory(Clock::System).unwrap();
    store.sign_in("sam@example.com", "Sam").unwrap();
    let (client, sync) = synced(&addr, &store, &household).await;
    (world, client, household, sync, store, dir)
}

#[tokio::test]
async fn a_failing_changes_call_surfaces_the_error() {
    let (_world, client, household, mut sync, store, _dir) = household_ready().await;

    sync.email_state = "e-fail".into();
    let err = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Email/changes"), "{err}");
    assert!(err.contains("serverFail"), "{err}");

    sync.email_state = "e0".into();
    sync.mailbox_state = "m-fail".into();
    let err = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Mailbox/changes"), "{err}");
}

#[tokio::test]
async fn a_quiet_poll_changes_nothing() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    let before = store.messages(Filter::Search("")).unwrap().len();

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert!(counts.is_quiet(), "{counts:?}");
    assert_eq!(store.messages(Filter::Search("")).unwrap().len(), before);
    assert_eq!(store.accounts().unwrap().len(), 1);
    drop(world);
}

#[tokio::test]
async fn new_inbox_mail_arrives_within_one_poll() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    world.lock().unwrap().add_email(
        HOUSEHOLD,
        email_json(
            "E-new",
            "T-new",
            ("City Water", "billing@citywater.gov"),
            "household",
            &["MB-in"],
            "Water main flush notice",
            Some("Flushing on Thursday."),
            "Flushing on Thursday",
        ),
    );

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert_eq!(counts.imported, 1, "{counts:?}");
    let arrived = the(&store, "E-new@chislan.family").unwrap();
    assert_eq!(arrived.values().unwrap().state, State::Inbox);
    assert_eq!(arrived.values().unwrap().folder, None);
}

#[tokio::test]
async fn edits_refresh_the_row_while_state_adoption_waits() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;

    // A folder picked up elsewhere refreshes the cached row.
    world
        .lock()
        .unwrap()
        .update_email(HOUSEHOLD, "E-roofer1", |mail| {
            mail["mailboxIds"]["MB-receipts"] = json!(true);
        });
    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();
    assert_eq!(counts.imported, 1, "{counts:?}");
    let roofer1 = the(&store, "E-roofer1@chislan.family").unwrap();
    assert_eq!(
        roofer1.values().unwrap().folder.as_deref(),
        Some("Receipts")
    );

    // A `Docket/` label added elsewhere is adoption's to apply, not the
    // poll's: Docket-owned state keeps what it holds.
    world
        .lock()
        .unwrap()
        .update_email(HOUSEHOLD, "E-roofer1", |mail| {
            mail["mailboxIds"]["MB-do"] = json!(true);
        });
    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();
    assert_eq!(counts.imported, 1, "{counts:?}");
    let roofer1 = the(&store, "E-roofer1@chislan.family").unwrap();
    assert_eq!(roofer1.values().unwrap().state, State::Inbox);
}

#[tokio::test]
async fn destroyed_mail_stays_cached() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    world.lock().unwrap().remove_email(HOUSEHOLD, "E-school");

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert_eq!(counts.destroyed, 1, "{counts:?}");
    assert_eq!(counts.imported, 0, "{counts:?}");
    assert!(!counts.is_quiet());
    assert!(the(&store, "E-school@chislan.family").is_some());
}

#[tokio::test]
async fn a_reimported_message_matches_by_message_id() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    world.lock().unwrap().remove_email(HOUSEHOLD, "E-roofer1");
    // The same message under a new server id, as a reimport or restore
    // brings it back.
    let mut again = email_json(
        "E-roofer1b",
        "T-roofer",
        ("Northwind Roofing", "office@northwind.co"),
        "household",
        &["MB-in"],
        "Gutter repair estimate",
        Some("Estimate attached: $1,840."),
        "Estimate attached",
    );
    again["messageId"] = json!("E-roofer1@chislan.family");
    world.lock().unwrap().add_email(HOUSEHOLD, again);

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert_eq!(counts.imported, 1, "{counts:?}");
    assert_eq!(store.messages(Filter::Search("")).unwrap().len(), 5);
    assert!(the(&store, "E-roofer1@chislan.family").is_some());
}

#[tokio::test]
async fn sent_replies_join_known_threads_only() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    world.lock().unwrap().add_email(
        HOUSEHOLD,
        email_json(
            "E-reply4",
            "T-roofer",
            ("Sam", "sam@example.com"),
            "office@northwind.co",
            &["MB-sent"],
            "Re: Gutter repair estimate",
            Some("Does the fascia need work too?"),
            "Does the fascia",
        ),
    );
    world.lock().unwrap().add_email(
        HOUSEHOLD,
        email_json(
            "E-solo",
            "T-unknown",
            ("Sam", "sam@example.com"),
            "someone@elsewhere.org",
            &["MB-sent"],
            "Re: unrelated",
            Some("Hello"),
            "Hello",
        ),
    );

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert_eq!(counts.imported, 1, "{counts:?}");
    let reply = the(&store, "E-reply4@chislan.family").unwrap();
    assert!(
        matches!(&reply.kind, Kind::Sent { by, .. } if by.as_deref() == Some("sam@example.com"))
    );
    assert!(the(&store, "E-solo@chislan.family").is_none());
}

#[tokio::test]
async fn a_disowned_state_triggers_a_full_reimport() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    sync.email_state = "e-bogus".into();

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert!(counts.resynced, "{counts:?}");
    assert_eq!(store.messages(Filter::Search("")).unwrap().len(), 5);

    // The fresh baseline carries forward: mail arriving after the
    // re-import still shows up on the next poll.
    world.lock().unwrap().add_email(
        HOUSEHOLD,
        email_json(
            "E-after",
            "T-after",
            ("City Water", "billing@citywater.gov"),
            "household",
            &["MB-in"],
            "Bill available",
            Some("Your bill is attached."),
            "Your bill",
        ),
    );
    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();
    assert_eq!(counts.imported, 1, "{counts:?}");
    assert!(the(&store, "E-after@chislan.family").is_some());
}

#[tokio::test]
async fn a_disowned_mailbox_state_reimports_too() {
    let (_world, client, household, mut sync, store, _dir) = household_ready().await;
    sync.mailbox_state = "m-bogus".into();

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert!(counts.resynced, "{counts:?}");
    assert_eq!(store.accounts().unwrap().len(), 1);
}

#[tokio::test]
async fn mailbox_changes_rebuild_the_layout() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    world.lock().unwrap().add_mailbox(
        HOUSEHOLD,
        json!({"id": "MB-travel", "name": "Travel", "role": null,
               "myRights": full_rights()}),
    );

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert!(counts.mailboxes, "{counts:?}");
    assert_eq!(store.folders().unwrap(), ["Receipts", "School", "Travel"]);

    // The rebuilt layout classifies against the new mailbox.
    world.lock().unwrap().add_email(
        HOUSEHOLD,
        email_json(
            "E-trip",
            "T-trip",
            ("Airline", "no-reply@airline.co"),
            "household",
            &["MB-in", "MB-travel"],
            "Itinerary",
            Some("Seats 14A and 14B."),
            "Seats 14A",
        ),
    );
    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();
    assert_eq!(counts.imported, 1, "{counts:?}");
    let trip = the(&store, "E-trip@chislan.family").unwrap();
    assert_eq!(trip.values().unwrap().folder.as_deref(), Some("Travel"));
}

#[tokio::test]
async fn many_changes_page_through() {
    let (world, client, household, mut sync, store, _dir) = household_ready().await;
    for n in 1..=60 {
        let id = format!("E-many{n:02}");
        world.lock().unwrap().add_email(
            HOUSEHOLD,
            email_json(
                &id,
                "T-many",
                ("Payer", "billing@payer.co"),
                "household",
                &["MB-in"],
                "Statement",
                Some("Balance due."),
                "Balance",
            ),
        );
    }

    let counts = client
        .poll_once(&household, &mut sync, &store)
        .await
        .unwrap();

    assert_eq!(counts.imported, 60, "{counts:?}");
    assert_eq!(store.messages(Filter::Search("")).unwrap().len(), 65);
}
