//! The JMAP side of Docket: one session per credential (ADR 1), with
//! rights derived from what the server permits rather than config. Token
//! scope surfaces in the session as capabilities and `isReadOnly`;
//! per-mailbox `myRights` report mailbox ACLs, so they gate per-mailbox
//! actions like filing (task rn), never account-wide scope.
//!
//! Keeping up with the server is polling for now (task lylzwoyo):
//! `poll_once` applies `Email/changes` and `Mailbox/changes` since the
//! states the last sync left, and drains the queued filings, archives,
//! and deletions (tasks rn and sm) with `Email/set` and
//! `Email/destroy`. Push replaces the timer later (task nwylszul); the
//! changes application stays.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::ErrorKind;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use jiff::Timestamp;
use serde::de;
use serde::de::DeserializeOwned;
use serde::de::value::MapAccessDeserializer;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;
use crate::model::{Account, MessageId, State, User};
use crate::store::{Incoming, IncomingKind, Store};

/// RFC 8620 §9. The capability URNs Docket looks for.
pub const CORE: &str = "urn:ietf:params:jmap:core";
pub const MAIL: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";

/// Where Fastmail serves the session document (DESIGN.md: Docket is
/// Fastmail-backed).
pub const FASTMAIL_SESSION_URL: &str = "https://api.fastmail.com/jmap/session";

#[derive(Debug, thiserror::Error)]
pub enum JmapError {
    #[error("reading the token file: {0}")]
    TokenFile(#[from] std::io::Error),

    #[error("JMAP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("malformed {what}")]
    Malformed {
        what: &'static str,
        #[source]
        source: serde_json::Error,
    },

    #[error("{method} failed: {detail}")]
    Reply {
        method: &'static str,
        detail: String,
    },

    #[error("{method} cannot calculate changes since the given state")]
    CannotCalculateChanges { method: &'static str },

    #[error("credential {credential:?} offers no mail account in its session")]
    NoMailAccount { credential: String },

    #[error("the token was rejected (401): wrong or revoked for this account")]
    Unauthorized,
}

/// A token's session, as the server reports it (RFC 8620 §3). Only what
/// Docket derives rights from is kept.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub api_url: String,
    /// accountId → account.
    pub accounts: BTreeMap<String, SessionAccount>,
    /// Capability → the accountId to use for it.
    pub primary_accounts: BTreeMap<String, String>,
}

impl Session {
    pub fn parse(json: &str) -> Result<Self, JmapError> {
        serde_json::from_str(json).map_err(|source| JmapError::Malformed {
            what: "session",
            source,
        })
    }

    /// The account this credential is for: the primary account for mail.
    /// With no sharing there is exactly one per session (ADR 1).
    pub fn mail_account(&self, credential: &str) -> Result<(&str, &SessionAccount), JmapError> {
        let missing = || JmapError::NoMailAccount {
            credential: credential.to_owned(),
        };
        let id = self.primary_accounts.get(MAIL).ok_or_else(missing)?;
        let account = self.accounts.get(id).ok_or_else(missing)?;
        Ok((id.as_str(), account))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionAccount {
    pub name: String,
    pub is_read_only: bool,
    /// Capability URN → its object; only the keys matter to Docket.
    pub account_capabilities: BTreeMap<String, Value>,
}

impl SessionAccount {
    /// What Docket offers on this account, per what the server permits:
    /// scope surfaces as capabilities and `isReadOnly`. `myRights` is
    /// deliberately absent — it reports mailbox ACLs, not scope (spike
    /// 2026-10-03).
    pub fn rights(&self) -> Rights {
        Rights {
            read_only: self.is_read_only,
            send: self.account_capabilities.contains_key(SUBMISSION),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rights {
    /// `isReadOnly` on the session account: no filing or replying.
    pub read_only: bool,
    /// The submission capability is offered: sending is on the table.
    pub send: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mailbox {
    pub id: String,
    pub name: String,
    pub role: Option<String>,
    pub my_rights: MailboxRights,
}

/// RFC 8621 §2.3: the ACLs on one mailbox. These reflect who may touch
/// that mailbox, not what the token's scopes permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxRights {
    pub may_read_items: bool,
    pub may_add_items: bool,
    pub may_remove_items: bool,
    pub may_set_keywords: bool,
    pub may_create_child: bool,
    pub may_rename: bool,
    pub may_delete: bool,
    pub may_submit: bool,
}

/// One JMAP request reply; `methodResponses` wraps each method's answer
/// (RFC 8620 §3.4). The client invoke id comes back untouched.
#[derive(Deserialize)]
struct Reply {
    #[serde(rename = "methodResponses")]
    method_responses: Vec<(String, Value, Value)>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Debug)]
struct Query {
    ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadEmails {
    email_ids: Vec<String>,
}

/// Pulls one page of ids out of an `Email/query` response.
fn parse_query(args: Value) -> Result<Query, JmapError> {
    serde_json::from_value(args).map_err(|source| JmapError::Malformed {
        what: "Email/query",
        source,
    })
}

/// Pulls the one method's answer out of its `methodResponses` envelope,
/// or names what went wrong instead.
fn parse_reply(method: &'static str, json: &str) -> Result<Value, JmapError> {
    let malformed = |source| JmapError::Malformed {
        what: method,
        source,
    };
    let reply: Reply = serde_json::from_str(json).map_err(malformed)?;
    let detail = |text: String| JmapError::Reply {
        method,
        detail: text,
    };
    match reply.method_responses.as_slice() {
        [(name, args, _)] => match name.as_str() {
            name if name == method => Ok(args.clone()),
            "error" => {
                let err: ApiError = serde_json::from_value(args.clone()).map_err(malformed)?;
                Err(if err.kind == "cannotCalculateChanges" {
                    JmapError::CannotCalculateChanges { method }
                } else {
                    match err.description {
                        Some(description) => detail(format!("{}: {description}", err.kind)),
                        None => detail(err.kind),
                    }
                })
            }
            other => Err(detail(format!("unexpected response {other:?}"))),
        },
        [] => Err(detail("the reply had no methodResponses".into())),
        _ => Err(detail("the reply had more than one methodResponse".into())),
    }
}

fn parse_list<T: DeserializeOwned>(method: &'static str, args: Value) -> Result<Vec<T>, JmapError> {
    #[derive(Deserialize)]
    struct Got<T> {
        list: Vec<T>,
    }
    serde_json::from_value::<Got<T>>(args)
        .map(|got| got.list)
        .map_err(|source| JmapError::Malformed {
            what: method,
            source,
        })
}

/// Pulls a list and its type state out of a `Foo/get` response: that
/// `state` is the `sinceState` the next `/changes` call starts from
/// (RFC 8620 §5.2).
fn parse_stateful_list<T: DeserializeOwned>(
    method: &'static str,
    args: Value,
) -> Result<(Vec<T>, String), JmapError> {
    #[derive(Deserialize)]
    struct Got<T> {
        state: String,
        list: Vec<T>,
    }
    serde_json::from_value::<Got<T>>(args)
        .map(|got| (got.list, got.state))
        .map_err(|source| JmapError::Malformed {
            what: method,
            source,
        })
}

/// One page of a `Foo/changes` reply (RFC 8620 §5.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Changes {
    has_more_changes: bool,
    new_state: String,
    created: Vec<String>,
    updated: Vec<String>,
    destroyed: Vec<String>,
}

impl Changes {
    /// Whether anything at all moved: even a destroy must rebuild the
    /// mailbox layout.
    fn changed(&self) -> bool {
        !(self.created.is_empty() && self.updated.is_empty() && self.destroyed.is_empty())
    }
}

fn parse_changes(method: &'static str, args: Value) -> Result<Changes, JmapError> {
    serde_json::from_value(args).map_err(|source| JmapError::Malformed {
        what: method,
        source,
    })
}

/// An RFC 5322 address as JMAP parses it (RFC 8621 §4.1.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmailAddress {
    pub name: Option<String>,
    pub email: String,
}

impl EmailAddress {
    /// The display form Docket shows: the name when there is one, else
    /// the bare address.
    pub fn display(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.email.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BodyPart {
    part_id: String,
    #[serde(rename = "type")]
    media_type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BodyValue {
    value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Email {
    pub id: String,
    thread_id: String,
    /// The Message-ID headers, without angle brackets (RFC 8621 §4.1:
    /// `String[]|null` — absent or repeated headers).
    pub message_id: Option<Vec<String>>,
    pub mailbox_ids: BTreeMap<String, bool>,
    pub received_at: Timestamp,
    /// `Date|null`: mail can carry no Date header, so it falls back to
    /// receivedAt at the call site.
    pub sent_at: Option<Timestamp>,
    pub subject: Option<String>,
    pub preview: Option<String>,
    /// All null when the header is absent (RFC 8621 §4.1).
    pub from: Option<Vec<EmailAddress>>,
    pub to: Option<Vec<EmailAddress>>,
    pub cc: Option<Vec<EmailAddress>>,
    pub bcc: Option<Vec<EmailAddress>>,
    pub text_body: Option<Vec<BodyPart>>,
    pub html_body: Option<Vec<BodyPart>>,
    pub body_values: Option<BTreeMap<String, BodyValue>>,
}

/// The message's text: its first plain-text part, then its first HTML
/// part converted to text, then the server's preview. A message with
/// none of those says so rather than rendering blank.
fn body_of(email: &Email) -> String {
    if let Some(plain) = first_part(email, email.text_body.as_deref(), "text/plain") {
        return plain.into();
    }
    if let Some(html) = html_of(email) {
        // from_read over a byte slice can't fail; the raw HTML is an
        // unreachable stand-in for that error.
        return html2text::from_read(html.as_bytes(), usize::MAX).unwrap_or_else(|_| html.into());
    }
    email
        .preview
        .as_deref()
        .filter(|preview| !preview.trim().is_empty())
        .map_or_else(|| "(no body)".into(), str::to_owned)
}

/// The message's first non-blank HTML part, raw. `htmlBody` falls back
/// to the text parts for text-only mail (RFC 8621 §4.1.4), so the media
/// type filter is what tells HTML mail apart.
fn html_of(email: &Email) -> Option<&str> {
    first_part(email, email.html_body.as_deref(), "text/html")
}

/// The first of `parts` of `media_type` whose fetched value isn't blank.
fn first_part<'a>(
    email: &'a Email,
    parts: Option<&'a [BodyPart]>,
    media_type: &str,
) -> Option<&'a str> {
    parts
        .unwrap_or_default()
        .iter()
        .filter(|part| part.media_type.starts_with(media_type))
        .filter_map(|part| {
            email
                .body_values
                .as_ref()
                .and_then(|values| values.get(&part.part_id))
        })
        .map(|value| value.value.as_str())
        .find(|value| !value.trim().is_empty())
}

/// Shapes a fetched email for the store. `None` skips a senderless
/// oddity (nothing to show for it).
fn incoming(email: &Email, layout: &Layout, users: &[User]) -> Option<Incoming> {
    let civil = |at: &Timestamp| at.to_zoned(jiff::tz::TimeZone::UTC).datetime();
    let names = |xs: &[EmailAddress]| xs.iter().map(EmailAddress::display).collect::<Vec<_>>();
    fn empty(xs: &Option<Vec<EmailAddress>>) -> &[EmailAddress] {
        xs.as_deref().unwrap_or_default()
    }
    let sent = layout.is_sent(email);
    let kind = if sent {
        IncomingKind::Sent {
            // Attribution by From address; the shared identity matches
            // no login and stays None.
            by: empty(&email.from).iter().find_map(|a| {
                users
                    .iter()
                    .find(|u| u.login == a.email)
                    .map(|u| u.login.clone())
            }),
            to: names(empty(&email.to)),
        }
    } else {
        let from = empty(&email.from).first()?;
        IncomingKind::Received {
            from: from.display(),
            addr: from.email.clone(),
            state: layout.state_of(email),
            folder: layout.folder_of(email),
        }
    };
    Some(Incoming {
        jmap_id: email.id.clone(),
        message_id: email
            .message_id
            .as_deref()
            .and_then(|ids| ids.first())
            .cloned()
            .unwrap_or_default(),
        jmap_thread_id: email.thread_id.clone(),
        subject: email
            .subject
            .clone()
            .unwrap_or_else(|| "(no subject)".into()),
        at: civil(if sent {
            email.sent_at.as_ref().unwrap_or(&email.received_at)
        } else {
            &email.received_at
        }),
        cc: names(empty(&email.cc)),
        bcc: names(empty(&email.bcc)),
        body: body_of(email),
        html: html_of(email).map(str::to_owned),
        kind,
    })
}

/// How Docket reads a session's mailboxes (DESIGN.md): roles pick out
/// the system boxes, the `Docket` namespace holds state labels, and the
/// role-less rest are folders.
#[derive(Debug, Clone, Default)]
struct Layout {
    /// The Inbox mailbox's id.
    pub inbox: Option<String>,
    /// The Sent mailbox's id.
    pub sent: Option<String>,
    /// The Archive mailbox's id: where Done lands.
    pub archive: Option<String>,
    /// The Trash mailbox's id: Done whatever labels it keeps.
    pub trash: Option<String>,
    /// Label mailbox id → the state it carries.
    labels: BTreeMap<String, State>,
    /// Mailbox ids in the Docket namespace: the labels and the parent.
    docket: BTreeSet<String>,
    /// Folder mailbox id → its name and ACLs.
    folders: BTreeMap<String, Folder>,
    /// Every mailbox's ACLs, for the moves that cross system boxes.
    rights: BTreeMap<String, MailboxRights>,
}

impl Layout {
    pub fn of(mailboxes: &[Mailbox]) -> Self {
        let mut layout = Self::default();
        // Labels may nest under a `Docket` parent instead of carrying
        // the prefix in their name.
        let parents: BTreeSet<&str> = mailboxes
            .iter()
            .filter(|m| m.name == "Docket")
            .map(|m| m.id.as_str())
            .collect();
        for mailbox in mailboxes {
            layout.rights.insert(mailbox.id.clone(), mailbox.my_rights);
            match mailbox.role.as_deref() {
                Some("inbox") => layout.inbox = Some(mailbox.id.clone()),
                Some("sent") => layout.sent = Some(mailbox.id.clone()),
                Some("archive") => layout.archive = Some(mailbox.id.clone()),
                Some("trash") => layout.trash = Some(mailbox.id.clone()),
                // The other system boxes (Drafts, Junk, ...) are
                // neither folders nor labels.
                Some(_) => {}
                None => {
                    let in_docket = mailbox.name == "Docket"
                        || mailbox.name.starts_with("Docket/")
                        || parents.contains(mailbox.id.as_str());
                    if in_docket {
                        layout.docket.insert(mailbox.id.clone());
                        let leaf = mailbox.name.rsplit('/').next().unwrap_or_default();
                        if let Some(state) = State::from_slug(&leaf.to_lowercase()) {
                            layout.labels.insert(mailbox.id.clone(), state);
                        }
                    } else {
                        layout.folders.insert(
                            mailbox.id.clone(),
                            Folder {
                                name: mailbox.name.clone(),
                                rights: mailbox.my_rights,
                            },
                        );
                    }
                }
            }
        }
        layout
    }

    /// The state the message's mailboxes say (DESIGN.md, State): Done
    /// in Trash whatever labels it keeps, else its `Docket/` label, else
    /// Inbox when it sits there, else Done.
    pub fn state_of(&self, email: &Email) -> State {
        let member = |id: &str| email.mailbox_ids.get(id) == Some(&true);
        if self.trash.as_deref().is_some_and(member) {
            return State::Done;
        }
        let label = email
            .mailbox_ids
            .iter()
            .filter(|(_, member)| **member)
            .find_map(|(id, _)| self.labels.get(id).copied());
        match label {
            Some(state) => state,
            None if self.is_inbox(email) => State::Inbox,
            None => State::Done,
        }
    }

    /// True when the message carries a `Docket/` label: state the poll
    /// follows even if Docket never saw it in the Inbox.
    pub fn is_labeled(&self, email: &Email) -> bool {
        email
            .mailbox_ids
            .iter()
            .any(|(id, member)| *member && self.labels.contains_key(id))
    }

    /// The folder the message is filed in: its role-less mailbox outside
    /// Docket, if it has one.
    pub fn folder_of(&self, email: &Email) -> Option<String> {
        email
            .mailbox_ids
            .keys()
            .find_map(|id| self.folders.get(id).map(|f| f.name.clone()))
    }

    /// True when the message sits in the Sent mailbox.
    pub fn is_sent(&self, email: &Email) -> bool {
        self.sent
            .as_deref()
            .is_some_and(|id| email.mailbox_ids.get(id) == Some(&true))
    }

    /// True when the message sits in the Inbox mailbox: what the poll
    /// path imports as received.
    pub fn is_inbox(&self, email: &Email) -> bool {
        self.inbox
            .as_deref()
            .is_some_and(|id| email.mailbox_ids.get(id) == Some(&true))
    }

    /// Folder names in the session's order, for the folders table.
    pub fn folder_names(&self) -> Vec<String> {
        self.folders.values().map(|f| f.name.clone()).collect()
    }

    /// The label mailbox id that carries `state`, if the server has
    /// one. Creating missing labels is task xy.
    fn label_of(&self, state: State) -> Option<&str> {
        self.labels
            .iter()
            .find(|(_, s)| **s == state)
            .map(|(id, _)| id.as_str())
    }

    /// The mailbox id a folder name files into.
    fn folder_id(&self, name: &str) -> Option<&str> {
        self.folders
            .iter()
            .find(|(_, f)| f.name == name)
            .map(|(id, _)| id.as_str())
    }
}

/// A role-less mailbox outside the Docket namespace: a folder, with
/// the ACLs that gate moving mail into and out of it.
#[derive(Debug, Clone)]
struct Folder {
    name: String,
    rights: MailboxRights,
}

/// The memberships a filing leaves the message in: out of every
/// folder, into the chosen one, with system boxes and `Docket/`
/// labels untouched (task rn's done-when).
fn filed_mailboxes(
    current: &BTreeMap<String, bool>,
    layout: &Layout,
    target: Option<&str>,
) -> BTreeMap<String, bool> {
    let mut next: BTreeMap<String, bool> = current
        .iter()
        .filter(|(id, member)| **member && !layout.folders.contains_key(*id))
        .map(|(id, _)| (id.clone(), true))
        .collect();
    if let Some(name) = target
        && let Some(id) = layout.folder_id(name)
    {
        next.insert(id.to_owned(), true);
    }
    next
}

/// Whether the ACLs permit the filing: the target takes items, and
/// every folder losing the message gives them up.
fn filing_allowed(current: &BTreeMap<String, bool>, layout: &Layout, target: Option<&str>) -> bool {
    let leaving = current
        .iter()
        .filter(|(id, member)| **member && layout.folders.contains_key(*id))
        .all(|(id, _)| {
            layout
                .folders
                .get(id)
                .is_some_and(|f| f.rights.may_remove_items)
        });
    let entering = target
        .and_then(|name| layout.folder_id(name))
        .is_none_or(|id| {
            layout
                .folders
                .get(id)
                .is_some_and(|f| f.rights.may_add_items)
        });
    leaving && entering
}

/// What a queued state move comes to against fresh memberships.
#[derive(Debug, PartialEq, Eq)]
enum Move {
    /// The memberships to write.
    To(BTreeMap<String, bool>),
    /// The mail already sits where the state says.
    Settled,
    /// The server lacks the mailbox the state lands in.
    Nowhere,
}

/// The memberships that say `state` (DESIGN.md, State): out of the
/// Inbox, Archive, and every label, then into the state's own mailbox
/// — the Inbox, its `Docket/` label, or Archive for Done — with
/// folders and other system boxes kept. Mail with neither Inbox nor
/// label already reads as Done, so Done leaves it where it is rather
/// than pulling filed mail into Archive.
fn moved_mailboxes(current: &BTreeMap<String, bool>, layout: &Layout, state: State) -> Move {
    let members: BTreeMap<String, bool> = current
        .iter()
        .filter(|(_, member)| **member)
        .map(|(id, _)| (id.clone(), true))
        .collect();
    let in_lane = |id: &str| Some(id) == layout.inbox.as_deref() || layout.labels.contains_key(id);
    let is_place = |id: &str| in_lane(id) || Some(id) == layout.archive.as_deref();
    if state == State::Done && !members.keys().any(|id| in_lane(id)) {
        return Move::Settled;
    }
    let target = match state {
        State::Inbox => layout.inbox.as_deref(),
        State::Done => layout.archive.as_deref(),
        State::Do | State::Wait | State::Watch => layout.label_of(state),
    };
    let Some(target) = target else {
        return Move::Nowhere;
    };
    let mut next: BTreeMap<String, bool> = members
        .iter()
        .filter(|(id, _)| !is_place(id))
        .map(|(id, _)| (id.clone(), true))
        .collect();
    next.insert(target.to_owned(), true);
    if next == members {
        Move::Settled
    } else {
        Move::To(next)
    }
}

/// Whether the ACLs permit the move: every mailbox losing the message
/// gives it up, and every one gaining it takes it.
fn move_allowed(
    current: &BTreeMap<String, bool>,
    next: &BTreeMap<String, bool>,
    layout: &Layout,
) -> bool {
    let leaving = current
        .iter()
        .filter(|(id, member)| **member && !next.contains_key(*id))
        .all(|(id, _)| {
            layout
                .rights
                .get(id)
                .is_none_or(|rights| rights.may_remove_items)
        });
    let entering = next
        .keys()
        .filter(|id| current.get(*id) != Some(&true))
        .all(|id| {
            layout
                .rights
                .get(id)
                .is_some_and(|rights| rights.may_add_items)
        });
    leaving && entering
}

/// Parses an `Email/set` reply's verdict on each id.
fn parse_set_emails(args: Value) -> Result<SetEmailsReply, JmapError> {
    serde_json::from_value(args).map_err(|source| JmapError::Malformed {
        what: "Email/set",
        source,
    })
}

/// Parses an `Email/destroy` reply's verdict on each id.
fn parse_destroy_emails(args: Value) -> Result<DestroyEmailsReply, JmapError> {
    serde_json::from_value(args).map_err(|source| JmapError::Malformed {
        what: "Email/destroy",
        source,
    })
}

/// How much mail one sync brought in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Imported {
    received: usize,
    sent: usize,
}

/// What the poll loop carries between cycles: the type states to call
/// `/changes` since, and the layout mail classifies against. Held in
/// memory — boot re-imports everything, so a restart re-derives it.
#[derive(Debug)]
pub struct Sync {
    pub email_state: String,
    pub mailbox_state: String,
    layout: Layout,
}

/// What one poll cycle did, for the loop's log line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PollCounts {
    /// Messages imported or refreshed.
    pub imported: usize,
    /// Destroyed ids seen. The rows stay cached — adopting destroys as
    /// Done is its own task.
    pub destroyed: usize,
    /// Whether the layout was rebuilt off `Mailbox/changes`.
    pub mailboxes: bool,
    /// Filings pushed server-side.
    pub filed: usize,
    /// State moves pushed server-side.
    pub moved: usize,
    /// Deletions pushed server-side.
    pub deleted: usize,
    /// Whether the server disowned the stored states and a full
    /// re-import ran instead.
    pub resynced: bool,
}

impl PollCounts {
    /// A cycle with nothing to show, logged at debug rather than info.
    pub fn is_quiet(&self) -> bool {
        *self == Self::default()
    }
}

// Every request Docket sends, as the RFCs shape it: one struct per
// method call, cited by section, so the wire format is
// compiler-checked and diffable against the spec in one place.

/// RFC 8620 §3.3: the request envelope for one method call.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Request<'a, A> {
    using: &'static [&'static str],
    method_calls: [(&'static str, &'a A, &'static str); 1],
}

/// `Mailbox/get` arguments (RFC 8621 §2.1).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetMailboxes<'a> {
    account_id: &'a str,
    /// Null fetches every mailbox (RFC 8620 §5.1).
    ids: Option<&'a [String]>,
    properties: &'static [MailboxProperty],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum MailboxProperty {
    Id,
    Name,
    Role,
    MyRights,
}

const MAILBOX_PROPERTIES: &[MailboxProperty] = &[
    MailboxProperty::Id,
    MailboxProperty::Name,
    MailboxProperty::Role,
    MailboxProperty::MyRights,
];

/// `Email/query` arguments (RFC 8621 §4.4).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QueryEmails<'a> {
    account_id: &'a str,
    filter: EmailFilterCondition<'a>,
    sort: [Comparator; 1],
    position: usize,
    limit: usize,
}

/// One filter term (RFC 8621 §4.4.1).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EmailFilterCondition<'a> {
    in_mailbox: &'a str,
}

/// A sort term (RFC 8620 §5.5); the properties Docket sorts by are in
/// RFC 8621 §4.4.2.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Comparator {
    property: EmailSortProperty,
    is_ascending: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum EmailSortProperty {
    ReceivedAt,
}

/// `Email/get` arguments (RFC 8621 §4.2).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetEmails<'a> {
    account_id: &'a str,
    ids: &'a [String],
    properties: &'static [EmailProperty],
    body_properties: &'static [BodyProperty],
    fetch_text_body_values: bool,
    #[serde(rename = "fetchHTMLBodyValues")]
    fetch_html_body_values: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum EmailProperty {
    Id,
    ThreadId,
    MessageId,
    MailboxIds,
    ReceivedAt,
    SentAt,
    Subject,
    From,
    To,
    Cc,
    Bcc,
    Preview,
    TextBody,
    HtmlBody,
    BodyValues,
}

const EMAIL_PROPERTIES: &[EmailProperty] = &[
    EmailProperty::Id,
    EmailProperty::ThreadId,
    EmailProperty::MessageId,
    EmailProperty::MailboxIds,
    EmailProperty::ReceivedAt,
    EmailProperty::SentAt,
    EmailProperty::Subject,
    EmailProperty::From,
    EmailProperty::To,
    EmailProperty::Cc,
    EmailProperty::Bcc,
    EmailProperty::Preview,
    EmailProperty::TextBody,
    EmailProperty::HtmlBody,
    EmailProperty::BodyValues,
];

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum BodyProperty {
    PartId,
    Type,
}

const BODY_PROPERTIES: &[BodyProperty] = &[BodyProperty::PartId, BodyProperty::Type];

/// `Thread/get` arguments (RFC 8621 §3.1).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetThreads<'a> {
    account_id: &'a str,
    ids: &'a [String],
    properties: &'static [ThreadProperty],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum ThreadProperty {
    Id,
    EmailIds,
}

const THREAD_PROPERTIES: &[ThreadProperty] = &[ThreadProperty::Id, ThreadProperty::EmailIds];

/// `Foo/changes` arguments (RFC 8620 §5.2).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GetChanges<'a> {
    account_id: &'a str,
    since_state: &'a str,
    max_changes: usize,
}

/// One page of request and response; small enough to keep bodies cheap.
const PAGE: usize = 50;

/// `Email/set` arguments (RFC 8621 §5): one patch per email.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetEmails<'a> {
    account_id: &'a str,
    update: &'a BTreeMap<String, EmailPatch>,
}

/// One email's patch: setting `mailboxIds` replaces the whole
/// membership set.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EmailPatch {
    mailbox_ids: BTreeMap<String, bool>,
}

/// The outcome half of an `Email/set` reply (RFC 8621 §5.3): which ids
/// took their patches, and which the server refused.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetEmailsReply {
    #[serde(default)]
    updated: BTreeMap<String, Option<Value>>,
    #[serde(default)]
    not_updated: BTreeMap<String, Value>,
}

/// `Email/destroy` arguments (RFC 8621 §5.5).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DestroyEmails<'a> {
    account_id: &'a str,
    destroy: &'a [String],
}

/// The outcome half of an `Email/destroy` reply: which ids went, and
/// which the server refused.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DestroyEmailsReply {
    #[serde(default)]
    destroyed: Vec<String>,
    #[serde(default)]
    not_destroyed: BTreeMap<String, Value>,
}

/// Opens sessions: one per credential, against Fastmail unless a test
/// points the session URL at a stub.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    session_url: String,
}

impl Client {
    pub fn fastmail() -> Result<Self, JmapError> {
        Self::new(FASTMAIL_SESSION_URL)
    }

    pub fn new(session_url: impl Into<String>) -> Result<Self, JmapError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()?,
            session_url: session_url.into(),
        })
    }

    async fn session(&self, token: &str) -> Result<Session, JmapError> {
        let response = self
            .http
            .get(&self.session_url)
            .bearer_auth(token)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(JmapError::Unauthorized);
        }
        let body = response.error_for_status()?.text().await?;
        Session::parse(&body)
    }

    /// Every mailbox of the account, with the rights the server reports
    /// for it, and the type state the next `/changes` starts from.
    async fn mailboxes(
        &self,
        session: &Session,
        token: &str,
        account_id: &str,
    ) -> Result<(Vec<Mailbox>, String), JmapError> {
        let args = self
            .call(
                &session.api_url,
                token,
                "Mailbox/get",
                &GetMailboxes {
                    account_id,
                    ids: None,
                    properties: MAILBOX_PROPERTIES,
                },
            )
            .await?;
        parse_stateful_list("Mailbox/get", args)
    }

    /// Posts one method call and returns the paired response's arguments.
    async fn call<A>(
        &self,
        api_url: &str,
        token: &str,
        method: &'static str,
        args: &A,
    ) -> Result<Value, JmapError>
    where
        A: Serialize + std::fmt::Debug,
    {
        let request = Request {
            using: &[CORE, MAIL],
            method_calls: [(method, args, "0")],
        };
        let body = self
            .http
            .post(api_url)
            .bearer_auth(token)
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        match parse_reply(method, &body) {
            Ok(args) => Ok(args),
            Err(error) => {
                // An invalidArguments reply names nothing; the request
                // Docket sent is the context it lacks.
                tracing::warn!(%method, ?args, %error, "JMAP call rejected");
                Err(error)
            }
        }
    }

    /// The named emails, with bodies, and the type state the next
    /// `/changes` starts from.
    async fn emails(
        &self,
        api_url: &str,
        token: &str,
        account_id: &str,
        ids: &[String],
    ) -> Result<(Vec<Email>, String), JmapError> {
        let args = self
            .call(
                api_url,
                token,
                "Email/get",
                &GetEmails {
                    account_id,
                    ids,
                    properties: EMAIL_PROPERTIES,
                    body_properties: BODY_PROPERTIES,
                    fetch_text_body_values: true,
                    fetch_html_body_values: true,
                },
            )
            .await?;
        parse_stateful_list("Email/get", args)
    }

    /// One page of `Foo/changes` since `since_state`.
    async fn changes(
        &self,
        api_url: &str,
        token: &str,
        account_id: &str,
        method: &'static str,
        since_state: &str,
    ) -> Result<Changes, JmapError> {
        let args = self
            .call(
                api_url,
                token,
                method,
                &GetChanges {
                    account_id,
                    since_state,
                    max_changes: PAGE,
                },
            )
            .await?;
        parse_changes(method, args)
    }

    /// Applies membership patches with one `Email/set`, returning the
    /// server's verdict on each id.
    async fn set_emails(
        &self,
        api_url: &str,
        token: &str,
        account_id: &str,
        update: &BTreeMap<String, EmailPatch>,
    ) -> Result<SetEmailsReply, JmapError> {
        let args = self
            .call(
                api_url,
                token,
                "Email/set",
                &SetEmails { account_id, update },
            )
            .await?;
        parse_set_emails(args)
    }

    /// Destroys emails with one `Email/destroy`, returning the
    /// server's verdict on each id.
    async fn destroy_emails(
        &self,
        api_url: &str,
        token: &str,
        account_id: &str,
        destroy: &[String],
    ) -> Result<DestroyEmailsReply, JmapError> {
        let args = self
            .call(
                api_url,
                token,
                "Email/destroy",
                &DestroyEmails {
                    account_id,
                    destroy,
                },
            )
            .await?;
        parse_destroy_emails(args)
    }

    /// Opens the credential's session, records its account, and imports
    /// its mail. What the server permits is the policy (ADR 1), and
    /// [`Import::account`] is the seam; the upserts underneath make this
    /// the refresh path too — rights and the mail cache re-read every
    /// time a session opens, while Docket-owned values (state,
    /// assignees, reads) keep whatever they already hold. Returns the
    /// states the import left the account in, for the poll loop.
    ///
    /// [`Import::account`]: crate::store::Import::account
    pub async fn sync_account(
        &self,
        credential: &Credential,
        store: &Store,
    ) -> Result<Sync, Error> {
        let token = read_token(&credential.token_file)?;
        let session = self.session(&token).await?;
        let (id, account) = session.mail_account(&credential.name)?;
        let rights = account.rights();
        let (mailboxes, mailbox_state) = self.mailboxes(&session, &token, id).await?;
        let layout = Layout::of(&mailboxes);
        let users = store.users()?;
        let folders = layout.folder_names();
        let mailbox_count = mailboxes.len();
        store.import(|tx| {
            tx.account(&Account {
                slug: credential.name.clone(),
                name: account.name.clone(),
                // Fastmail accountIds are the login's address (spike
                // 2026-10-03).
                address: id.to_owned(),
                read_only: rights.read_only,
            })?;
            for folder in &folders {
                tx.folder(folder)?;
            }
            Ok(())
        })?;
        let mut email_state = None;
        let imported = self
            .import_mail(
                &session,
                &token,
                id,
                &credential.name,
                &layout,
                &users,
                store,
                &mut email_state,
            )
            .await?;
        // An Inbox-less account never fetched a page, so no Email/get
        // reported a state; ask for nothing to learn one.
        let email_state = match email_state {
            Some(state) => state,
            None => self.emails(&session.api_url, &token, id, &[]).await?.1,
        };
        tracing::info!(
            credential = %credential.name,
            account = %id,
            read_only = rights.read_only,
            send = rights.send,
            mailboxes = mailbox_count,
            received = imported.received,
            sent = imported.sent,
            "session opened",
        );
        Ok(Sync {
            email_state,
            mailbox_state,
            layout,
        })
    }

    /// The account's Inbox mail plus our sent replies in its threads:
    /// received messages land in Inbox — or the state their `Docket/`
    /// label already carries — unassigned; sent ones carry a login when
    /// the From address names one of us, else the shared identity.
    /// Filed and archived mail stays out; once mail is tracked, the
    /// poll follows it out of the Inbox (task qwt).
    #[allow(clippy::too_many_arguments)]
    async fn import_mail(
        &self,
        session: &Session,
        token: &str,
        account_id: &str,
        slug: &str,
        layout: &Layout,
        users: &[User],
        store: &Store,
        email_state: &mut Option<String>,
    ) -> Result<Imported, Error> {
        let mut imported = Imported::default();
        let Some(inbox) = layout.inbox.clone() else {
            return Ok(imported);
        };
        // The Inbox pass drives everything: its threads decide which
        // sent replies belong.
        let mut threads = BTreeSet::new();
        let mut known = BTreeSet::new();
        let mut position = 0usize;
        loop {
            let args = self
                .call(
                    &session.api_url,
                    token,
                    "Email/query",
                    &QueryEmails {
                        account_id,
                        filter: EmailFilterCondition { in_mailbox: &inbox },
                        sort: [Comparator {
                            property: EmailSortProperty::ReceivedAt,
                            is_ascending: false,
                        }],
                        position,
                        limit: PAGE,
                    },
                )
                .await?;
            let page = parse_query(args)?;
            let n = page.ids.len();
            if n == 0 {
                break;
            }
            let (emails, state) = self
                .emails(&session.api_url, token, account_id, &page.ids)
                .await?;
            // The first page's state is the baseline the poll loop
            // starts from: mail landing mid-import is then changed-since
            // this state, so the first poll sweeps it in.
            if email_state.is_none() {
                *email_state = Some(state);
            }
            for email in &emails {
                known.insert(email.id.clone());
                threads.insert(email.thread_id.clone());
                if let Some(mail) = incoming(email, layout, users) {
                    store.import(|tx| tx.incoming(slug, &mail))?;
                    imported.received = imported.received.saturating_add(1);
                }
            }
            position = position.saturating_add(n);
        }

        // Threads bring their sent replies with them: Thread/get lists
        // every email in each, and the Sent-mailbox ones import. Reply
        // volume stays bounded by the threads the Inbox pass pulled.
        let mut wanted = Vec::new();
        let thread_ids: Vec<String> = threads.into_iter().collect();
        for chunk in thread_ids.chunks(PAGE) {
            let args = self
                .call(
                    &session.api_url,
                    token,
                    "Thread/get",
                    &GetThreads {
                        account_id,
                        ids: chunk,
                        properties: THREAD_PROPERTIES,
                    },
                )
                .await?;
            for thread in parse_list::<ThreadEmails>("Thread/get", args)? {
                wanted.extend(
                    thread
                        .email_ids
                        .into_iter()
                        .filter(|id| !known.contains(id)),
                );
            }
        }
        for chunk in wanted.chunks(PAGE) {
            let (emails, _) = self
                .emails(&session.api_url, token, account_id, chunk)
                .await?;
            for email in &emails {
                if layout.is_sent(email)
                    && let Some(mail) = incoming(email, layout, users)
                {
                    store.import(|tx| tx.incoming(slug, &mail))?;
                    imported.sent = imported.sent.saturating_add(1);
                }
            }
        }
        Ok(imported)
    }

    /// One poll cycle: `/changes` for mailboxes and email since the
    /// states the account was left in, applying what moved, then the
    /// queued filings, state moves, and deletions go out. A
    /// `cannotCalculateChanges` reply means the server disowns those
    /// states (RFC 8620 §5.2), so the cycle re-imports from scratch
    /// and the caller carries the fresh states forward.
    pub async fn poll_once(
        &self,
        credential: &Credential,
        sync: &mut Sync,
        store: &Store,
    ) -> Result<PollCounts, Error> {
        let token = read_token(&credential.token_file)?;
        let session = self.session(&token).await?;
        let (id, account) = session.mail_account(&credential.name)?;
        let mut counts = PollCounts::default();

        let mailboxes = match self
            .changes(
                &session.api_url,
                &token,
                id,
                "Mailbox/changes",
                &sync.mailbox_state,
            )
            .await
        {
            Ok(page) => page,
            Err(JmapError::CannotCalculateChanges { .. }) => {
                *sync = self.sync_account(credential, store).await?;
                counts.resynced = true;
                return Ok(counts);
            }
            Err(other) => return Err(other.into()),
        };
        // An unchanged reply still advances the state (RFC 8620 §5.2).
        sync.mailbox_state = mailboxes.new_state.clone();
        if mailboxes.changed() {
            let (mailboxes, mailbox_state) = self.mailboxes(&session, &token, id).await?;
            sync.layout = Layout::of(&mailboxes);
            sync.mailbox_state = mailbox_state;
            let rights = account.rights();
            let slug = credential.name.clone();
            let name = account.name.clone();
            let address = id.to_owned();
            let folders = sync.layout.folder_names();
            store.import(|tx| {
                tx.account(&Account {
                    slug,
                    name,
                    address,
                    read_only: rights.read_only,
                })?;
                for folder in &folders {
                    tx.folder(folder)?;
                }
                Ok(())
            })?;
            counts.mailboxes = true;
        }

        counts.filed = self
            .push_files(&session, &token, id, &credential.name, &sync.layout, store)
            .await?;
        counts.moved = self
            .push_moves(&session, &token, id, &credential.name, &sync.layout, store)
            .await?;
        counts.deleted = self
            .push_deletes(&session, &token, id, &credential.name, store)
            .await?;

        loop {
            let page = match self
                .changes(
                    &session.api_url,
                    &token,
                    id,
                    "Email/changes",
                    &sync.email_state,
                )
                .await
            {
                Ok(page) => page,
                Err(JmapError::CannotCalculateChanges { .. }) => {
                    *sync = self.sync_account(credential, store).await?;
                    counts.resynced = true;
                    return Ok(counts);
                }
                Err(other) => return Err(other.into()),
            };
            let Changes {
                has_more_changes,
                new_state,
                created,
                updated,
                destroyed,
            } = page;
            if !destroyed.is_empty() {
                // Rows stay cached; adopting destroys as Done is its own
                // task.
                tracing::debug!(?destroyed, "mail destroyed server-side");
            }
            counts.destroyed = counts.destroyed.saturating_add(destroyed.len());
            let mut ids = created;
            ids.extend(updated);
            ids.sort();
            ids.dedup();
            if !ids.is_empty() {
                let (emails, _) = self.emails(&session.api_url, &token, id, &ids).await?;
                let users = store.users()?;
                // Received first: a new Inbox message founds the thread
                // its same-batch sent reply then lands in. Labeled mail
                // is Docket's too, and mail already tracked stays
                // followed out of the Inbox: leaving is a state change
                // (task qwt).
                for email in &emails {
                    let Some(mail) = incoming(email, &sync.layout, &users) else {
                        continue;
                    };
                    let followed = !sync.layout.is_sent(email)
                        && (sync.layout.is_labeled(email)
                            || store.has_message(&credential.name, &mail.message_id)?);
                    if sync.layout.is_inbox(email) || followed {
                        store.import(|tx| tx.incoming(&credential.name, &mail))?;
                        counts.imported = counts.imported.saturating_add(1);
                    }
                }
                for email in &emails {
                    if sync.layout.is_sent(email)
                        && store.has_thread(&credential.name, &email.thread_id)?
                        && let Some(mail) = incoming(email, &sync.layout, &users)
                    {
                        store.import(|tx| tx.incoming(&credential.name, &mail))?;
                        counts.imported = counts.imported.saturating_add(1);
                    }
                }
            }
            sync.email_state = new_state;
            if !has_more_changes {
                break;
            }
        }
        Ok(counts)
    }

    /// Drains the account's queued filings (task rn): each message's
    /// memberships move it out of whatever folders hold it into the
    /// chosen one, leaving system boxes and `Docket/` labels untouched.
    /// Mailbox ACLs decide — entries the rights refuse stay queued and
    /// nag the log until an admin grants them; ids the server refused
    /// or no longer carries are dropped with a warning instead of
    /// retried forever.
    async fn push_files(
        &self,
        session: &Session,
        token: &str,
        account_id: &str,
        slug: &str,
        layout: &Layout,
        store: &Store,
    ) -> Result<usize, Error> {
        let pending = store.pending_files(slug)?;
        if pending.is_empty() {
            return Ok(0);
        }
        // Memberships come fresh from the server: the store keeps only
        // the folder they derive from.
        let mut current: BTreeMap<String, BTreeMap<String, bool>> = BTreeMap::new();
        for chunk in pending.chunks(PAGE) {
            let ids: Vec<String> = chunk.iter().map(|p| p.jmap_id.clone()).collect();
            let (emails, _) = self
                .emails(&session.api_url, token, account_id, &ids)
                .await?;
            for email in emails {
                current.insert(email.id.clone(), email.mailbox_ids);
            }
        }
        let mut updates: BTreeMap<String, EmailPatch> = BTreeMap::new();
        let mut clear: Vec<MessageId> = Vec::new();
        for p in &pending {
            let Some(memberships) = current.get(&p.jmap_id) else {
                tracing::warn!(jmap_id = %p.jmap_id, "filing dropped: the mail is gone server-side");
                clear.push(p.message.clone());
                continue;
            };
            if p.folder
                .as_deref()
                .is_some_and(|f| layout.folder_id(f).is_none())
            {
                tracing::warn!(?p.folder, "filing dropped: the folder vanished server-side");
                clear.push(p.message.clone());
                continue;
            }
            if !filing_allowed(memberships, layout, p.folder.as_deref()) {
                tracing::warn!(?p.folder, "filing held: mailbox ACLs refuse it");
                continue;
            }
            updates.insert(
                p.jmap_id.clone(),
                EmailPatch {
                    mailbox_ids: filed_mailboxes(memberships, layout, p.folder.as_deref()),
                },
            );
        }
        let mut filed = 0usize;
        let entries: Vec<(String, EmailPatch)> = updates.into_iter().collect();
        for chunk in entries.chunks(PAGE) {
            let update: BTreeMap<String, EmailPatch> = chunk.iter().cloned().collect();
            let reply = self
                .set_emails(&session.api_url, token, account_id, &update)
                .await?;
            for (id, error) in &reply.not_updated {
                tracing::error!(jmap_id = %id, %error, "the server refused a filing");
            }
            filed = filed.saturating_add(reply.updated.len());
            // Refused ids clear too: the server's verdict stands, and
            // retrying would only requeue the refusal.
            clear.extend(chunk.iter().filter_map(|(id, _)| {
                pending
                    .iter()
                    .find(|p| &p.jmap_id == id)
                    .map(|p| p.message.clone())
            }));
        }
        store.clear_pending_files(&clear)?;
        Ok(filed)
    }

    /// Drains the queued state moves: each message's memberships move
    /// to the mailboxes that say its state (DESIGN.md, State), folders
    /// kept. Self-limiting against fresh memberships: mail already
    /// where its state says is left alone, its intent cleared silently.
    /// ACL-refused entries stay queued and nag the log until an admin
    /// grants them; server refusals, ids the server no longer carries,
    /// and states whose mailbox the server lacks are dropped with a
    /// warning instead of retried forever.
    async fn push_moves(
        &self,
        session: &Session,
        token: &str,
        account_id: &str,
        slug: &str,
        layout: &Layout,
        store: &Store,
    ) -> Result<usize, Error> {
        let pending = store.pending_moves(slug)?;
        if pending.is_empty() {
            return Ok(0);
        }
        // Memberships come fresh from the server: the store keeps only
        // the folder they derive from.
        let mut current: BTreeMap<String, BTreeMap<String, bool>> = BTreeMap::new();
        for chunk in pending.chunks(PAGE) {
            let ids: Vec<String> = chunk.iter().map(|p| p.jmap_id.clone()).collect();
            let (emails, _) = self
                .emails(&session.api_url, token, account_id, &ids)
                .await?;
            for email in emails {
                current.insert(email.id.clone(), email.mailbox_ids);
            }
        }
        let mut updates: BTreeMap<String, EmailPatch> = BTreeMap::new();
        let mut clear: Vec<MessageId> = Vec::new();
        for p in &pending {
            let Some(memberships) = current.get(&p.jmap_id) else {
                tracing::warn!(jmap_id = %p.jmap_id, "move dropped: the mail is gone server-side");
                clear.push(p.message.clone());
                continue;
            };
            let next = match moved_mailboxes(memberships, layout, p.state) {
                Move::To(next) => next,
                Move::Settled => {
                    clear.push(p.message.clone());
                    continue;
                }
                Move::Nowhere => {
                    // Leaving the Inbox with nowhere to land would read
                    // as Done server-side, so the mail stays put.
                    tracing::warn!(jmap_id = %p.jmap_id, state = %p.state, "move dropped: the server has no mailbox for the state");
                    clear.push(p.message.clone());
                    continue;
                }
            };
            if !move_allowed(memberships, &next, layout) {
                tracing::warn!(state = %p.state, "move held: mailbox ACLs refuse it");
                continue;
            }
            updates.insert(p.jmap_id.clone(), EmailPatch { mailbox_ids: next });
        }
        let mut moved = 0usize;
        let entries: Vec<(String, EmailPatch)> = updates.into_iter().collect();
        for chunk in entries.chunks(PAGE) {
            let update: BTreeMap<String, EmailPatch> = chunk.iter().cloned().collect();
            let reply = self
                .set_emails(&session.api_url, token, account_id, &update)
                .await?;
            for (id, error) in &reply.not_updated {
                tracing::error!(jmap_id = %id, %error, "the server refused a move");
            }
            moved = moved.saturating_add(reply.updated.len());
            // Refused ids clear too: the server's verdict stands, and
            // retrying would only requeue the refusal.
            clear.extend(chunk.iter().filter_map(|(id, _)| {
                pending
                    .iter()
                    .find(|p| &p.jmap_id == id)
                    .map(|p| p.message.clone())
            }));
        }
        store.clear_pending_moves(&clear)?;
        Ok(moved)
    }

    /// Drains the account's queued deletions (task sm) with
    /// `Email/destroy` — Fastmail's answer to a destroy is moving the
    /// mail to Trash. Ids the server refused or no longer carries are
    /// dropped with a warning rather than retried; the thread already
    /// went Done when the deletion was queued.
    async fn push_deletes(
        &self,
        session: &Session,
        token: &str,
        account_id: &str,
        slug: &str,
        store: &Store,
    ) -> Result<usize, Error> {
        let pending = store.pending_deletes(slug)?;
        if pending.is_empty() {
            return Ok(0);
        }
        let mut deleted = 0usize;
        let mut clear = Vec::new();
        let ids: Vec<String> = pending.iter().map(|p| p.jmap_id.clone()).collect();
        for chunk in ids.chunks(PAGE) {
            let reply = self
                .destroy_emails(&session.api_url, token, account_id, chunk)
                .await?;
            for (id, error) in &reply.not_destroyed {
                tracing::error!(jmap_id = %id, %error, "the server refused a deletion");
            }
            deleted = deleted.saturating_add(reply.destroyed.len());
            // Refused ids clear too: the server's verdict stands, and
            // retrying would only requeue the refusal.
            clear.extend(chunk.iter().filter_map(|id| {
                pending
                    .iter()
                    .find(|p| &p.jmap_id == id)
                    .map(|p| p.message.clone())
            }));
        }
        store.clear_pending_deletes(&clear)?;
        Ok(deleted)
    }
}

/// The token stays in its file (DESIGN.md, Storage), read fresh each
/// session so rotation needs no restart.
fn read_token(path: &Utf8Path) -> Result<String, JmapError> {
    let token = fs_err::read_to_string(path)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(std::io::Error::new(ErrorKind::InvalidData, format!("{path} is empty")).into());
    }
    Ok(token.to_owned())
}

/// One Fastmail API token: a name in `docket.kdl`, and the file holding
/// the token. The name doubles as the account's slug.
#[derive(Debug, Clone)]
pub struct Credential {
    pub name: String,
    pub token_file: Utf8PathBuf,
}

/// The kdl field names of a `credential` node: the first positional
/// argument (`#0`), a would-be second (`#1`, rejected), and the property.
const CREDENTIAL_FIELDS: &[&str] = &["#0", "#1", "token-file"];

impl<'de> Deserialize<'de> for Credential {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = Credential;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a credential node")
            }

            fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Credential, A::Error> {
                let mut name = None;
                let mut token_file = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "#0" => name = map.next_value::<Option<String>>()?,
                        "token-file" => token_file = map.next_value::<Option<String>>()?,
                        "#1" => {
                            return Err(de::Error::custom(
                                "a credential takes one argument, its name",
                            ));
                        }
                        other => {
                            return Err(de::Error::custom(format!(
                                "unexpected property `{other}`"
                            )));
                        }
                    }
                }
                let name = name.ok_or_else(|| {
                    de::Error::custom(
                        "a credential is named by its argument: \
                         credential \"household\" token-file=…",
                    )
                })?;
                let token_file = token_file
                    .ok_or_else(|| de::Error::custom("a credential needs a token-file property"))?;
                Ok(Credential {
                    name,
                    token_file: Utf8PathBuf::from(token_file),
                })
            }
        }
        deserializer.deserialize_struct("Credential", CREDENTIAL_FIELDS, V)
    }
}

/// The `credential` nodes of `docket.kdl`, flat per DESIGN.md:
///
/// ```kdl
/// credential "household" token-file="/run/credentials/docket/household"
/// ```
///
/// Hand-written because kdl's serde mapping feeds a lone same-named node
/// to the field directly but a group of them as a sequence, so the type
/// has to accept both shapes.
#[derive(Debug, Clone, Default)]
pub struct Credentials(pub Vec<Credential>);

impl std::ops::Deref for Credentials {
    type Target = [Credential];

    fn deref(&self) -> &[Credential] {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Credentials {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = Credentials;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("credential nodes")
            }

            fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Credentials, A::Error> {
                let mut all = Vec::new();
                while let Some(credential) = seq.next_element::<Credential>()? {
                    all.push(credential);
                }
                Ok(Credentials(all))
            }

            // A lone `credential` node: the map is the node's own
            // arguments and properties, so it is one Credential.
            fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Credentials, A::Error> {
                Credential::deserialize(MapAccessDeserializer::new(map))
                    .map(|credential| Credentials(vec![credential]))
            }
        }
        deserializer.deserialize_struct("Credentials", CREDENTIAL_FIELDS, V)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn requests_serialize_to_the_spec_shapes() {
        let mailboxes = serde_json::to_value(GetMailboxes {
            account_id: "a",
            ids: None,
            properties: MAILBOX_PROPERTIES,
        })
        .unwrap();
        assert_eq!(
            mailboxes,
            json!({"accountId": "a", "ids": null,
                   "properties": ["id", "name", "role", "myRights"]})
        );

        let query = serde_json::to_value(QueryEmails {
            account_id: "a",
            filter: EmailFilterCondition {
                in_mailbox: "inbox",
            },
            sort: [Comparator {
                property: EmailSortProperty::ReceivedAt,
                is_ascending: false,
            }],
            position: 0,
            limit: PAGE,
        })
        .unwrap();
        assert_eq!(
            query,
            json!({"accountId": "a", "filter": {"inMailbox": "inbox"},
                   "sort": [{"property": "receivedAt", "isAscending": false}],
                   "position": 0, "limit": PAGE})
        );

        let emails = serde_json::to_value(GetEmails {
            account_id: "a",
            ids: &["e1".into()],
            properties: EMAIL_PROPERTIES,
            body_properties: BODY_PROPERTIES,
            fetch_text_body_values: true,
            fetch_html_body_values: true,
        })
        .unwrap();
        assert_eq!(
            emails,
            json!({"accountId": "a", "ids": ["e1"],
                   "properties": ["id", "threadId", "messageId", "mailboxIds",
                       "receivedAt", "sentAt", "subject", "from", "to", "cc", "bcc",
                       "preview", "textBody", "htmlBody", "bodyValues"],
                   "bodyProperties": ["partId", "type"],
                   "fetchTextBodyValues": true, "fetchHTMLBodyValues": true})
        );

        let threads = serde_json::to_value(GetThreads {
            account_id: "a",
            ids: &["t1".into()],
            properties: THREAD_PROPERTIES,
        })
        .unwrap();
        assert_eq!(
            threads,
            json!({"accountId": "a", "ids": ["t1"], "properties": ["id", "emailIds"]})
        );

        let changes = serde_json::to_value(GetChanges {
            account_id: "a",
            since_state: "s1",
            max_changes: PAGE,
        })
        .unwrap();
        assert_eq!(
            changes,
            json!({"accountId": "a", "sinceState": "s1", "maxChanges": PAGE})
        );
    }

    /// A session shaped like Fastmail's: `read_only` is a mail-only token
    /// (the spike's findings), otherwise mail and submission.
    fn session(read_only: bool) -> String {
        let id = if read_only {
            "eli@example.com"
        } else {
            "household@example.com"
        };
        let mut account = json!({
            "name": if read_only { "Eli" } else { "Household" },
            "isPersonal": true,
            "isReadOnly": read_only,
            "accountCapabilities": {
                CORE: {},
                MAIL: {},
            },
        });
        let mut primary = json!({ MAIL: id });
        if !read_only {
            account["accountCapabilities"][SUBMISSION] = json!({});
            primary[SUBMISSION] = json!(id);
        }
        json!({
            "username": id,
            "apiUrl": "https://api.fastmail.com/jmap/api/",
            "capabilities": { CORE: {}, MAIL: {} },
            "accounts": { id: account },
            "primaryAccounts": primary,
        })
        .to_string()
    }

    #[test]
    fn a_writable_session_offers_mail_and_submission() {
        let session = Session::parse(&session(false)).unwrap();
        let (id, account) = session.mail_account("household").unwrap();
        assert_eq!(id, "household@example.com");
        assert_eq!(account.name, "Household");
        assert_eq!(
            account.rights(),
            Rights {
                read_only: false,
                send: true,
            }
        );
    }

    #[test]
    fn a_read_only_session_offers_mail_only() {
        let session = Session::parse(&session(true)).unwrap();
        let (id, account) = session.mail_account("eli").unwrap();
        assert_eq!(id, "eli@example.com");
        assert!(!account.account_capabilities.contains_key(SUBMISSION));
        assert_eq!(
            account.rights(),
            Rights {
                read_only: true,
                send: false,
            }
        );
    }

    #[test]
    fn a_session_without_a_mail_account_is_an_error() {
        let empty = json!({"apiUrl": "u", "accounts": {}, "primaryAccounts": {}}).to_string();
        let err = Session::parse(&empty)
            .unwrap()
            .mail_account("household")
            .unwrap_err();
        assert!(err.to_string().contains("household"), "{err}");
    }

    #[allow(clippy::too_many_arguments)]
    fn rights(
        read: bool,
        add: bool,
        remove: bool,
        keywords: bool,
        create_child: bool,
        rename: bool,
        delete: bool,
        submit: bool,
    ) -> Value {
        json!({
            "mayReadItems": read,
            "mayAddItems": add,
            "mayRemoveItems": remove,
            "maySetKeywords": keywords,
            "mayCreateChild": create_child,
            "mayRename": rename,
            "mayDelete": delete,
            "maySubmit": submit,
        })
    }

    fn mailbox(id: &str, name: &str, role: Option<&str>, my_rights: Value) -> Value {
        json!({"id": id, "name": name, "role": role, "myRights": my_rights})
    }

    #[test]
    fn mailboxes_unwrap_the_method_responses() {
        let reply = json!({
            "methodResponses": [[
                "Mailbox/get",
                {"accountId": "a", "state": "s", "notFound": [], "list": [
                    mailbox("M1", "Inbox", Some("inbox"),
                            rights(true, true, true, true, false, false, false, true)),
                    mailbox("M2", "Receipts", None,
                            rights(true, true, true, true, true, true, true, true)),
                    // Server-managed, so restricted even for the owner
                    // (spike 2026-10-03).
                    mailbox("M3", "Scheduled", Some("scheduled"),
                            rights(true, false, false, false, false, false, false, false)),
                ]},
                "0",
            ]],
        })
        .to_string();
        let args = parse_reply("Mailbox/get", &reply).unwrap();
        let mailboxes = parse_list::<Mailbox>("Mailbox/get", args).unwrap();
        let names: Vec<_> = mailboxes.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Inbox", "Receipts", "Scheduled"]);
        assert_eq!(mailboxes.first().unwrap().role.as_deref(), Some("inbox"));
        assert_eq!(mailboxes.get(1).unwrap().role, None);
        let scheduled = mailboxes.get(2).unwrap();
        assert!(scheduled.my_rights.may_read_items);
        assert!(!scheduled.my_rights.may_add_items);
        assert!(!scheduled.my_rights.may_submit);
    }

    #[test]
    fn a_list_that_wont_parse_fails_its_method() {
        let args = json!({"accountId": "a", "list": [{"id": 3}]});
        let err = parse_list::<Mailbox>("Mailbox/get", args).unwrap_err();
        assert!(err.to_string().contains("malformed Mailbox/get"), "{err}");
    }

    #[test]
    fn an_error_reply_fails_the_mailbox_fetch() {
        let reply = json!({
            "methodResponses": [[
                "error",
                {"type": "serverFail", "description": "boom"},
                "0",
            ]],
        })
        .to_string();
        let err = parse_reply("Mailbox/get", &reply).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Mailbox/get"), "{text}");
        assert!(text.contains("serverFail: boom"), "{text}");

        let undescribed = json!({
            "methodResponses": [["error", {"type": "serverFail"}, "0"]],
        })
        .to_string();
        assert!(
            parse_reply("Mailbox/get", &undescribed)
                .unwrap_err()
                .to_string()
                .contains("serverFail")
        );
    }

    #[test]
    fn a_reply_that_is_not_the_method_asked_for_fails() {
        let wrong = json!({"methodResponses": [["Email/get", {"list": []}, "0"]]}).to_string();
        assert!(
            parse_reply("Mailbox/get", &wrong)
                .unwrap_err()
                .to_string()
                .contains("unexpected response")
        );

        let doubled = json!({
            "methodResponses": [
                ["Mailbox/get", {"list": []}, "0"],
                ["Mailbox/get", {"list": []}, "1"],
            ],
        })
        .to_string();
        assert!(
            parse_reply("Mailbox/get", &doubled)
                .unwrap_err()
                .to_string()
                .contains("more than one methodResponse")
        );
    }

    #[test]
    fn malformed_replies_fail_to_parse() {
        assert!(Session::parse("{").is_err());
        assert!(parse_reply("Mailbox/get", "[]").is_err());
        assert!(parse_reply("Mailbox/get", r#"{"methodResponses": []}"#).is_err());
    }

    /// The mailbox set a household account carries: system boxes by role,
    /// `Docket/` labels by name, everything role-less as folders.
    fn mailboxes() -> Vec<Mailbox> {
        let full = rights(true, true, true, true, true, true, true, true);
        [
            ("M-in", "Inbox", Some("inbox")),
            ("M-sent", "Sent", Some("sent")),
            ("M-archive", "Archive", Some("archive")),
            ("M-do", "Docket/Do", None),
            ("M-watch", "Docket/Watch", None),
            ("M-receipts", "Receipts", None),
            ("M-school", "School", None),
        ]
        .into_iter()
        .map(|(id, name, role)| {
            serde_json::from_value::<Mailbox>(mailbox(id, name, role, full.clone())).unwrap()
        })
        .collect()
    }

    #[test]
    fn filings_move_folders_and_leave_labels_alone() {
        let layout = Layout::of(&mailboxes());
        let current = BTreeMap::from([
            ("M-in".to_string(), true),
            ("M-watch".to_string(), true),
            ("M-school".to_string(), true),
        ]);
        let filed = filed_mailboxes(&current, &layout, Some("Receipts"));
        assert_eq!(
            filed,
            BTreeMap::from([
                ("M-in".to_string(), true),
                ("M-watch".to_string(), true),
                ("M-receipts".to_string(), true),
            ])
        );

        // Unfiling drops every folder membership, nothing else.
        let unfiled = filed_mailboxes(&current, &layout, None);
        assert_eq!(
            unfiled,
            BTreeMap::from([("M-in".to_string(), true), ("M-watch".to_string(), true),])
        );

        // A folder the layout lost — renamed or removed server-side —
        // files nothing, but the rest still comes through untouched.
        let vanished = filed_mailboxes(&current, &layout, Some("Gone"));
        assert_eq!(
            vanished,
            BTreeMap::from([("M-in".to_string(), true), ("M-watch".to_string(), true),])
        );
    }

    #[test]
    fn mailbox_acls_decide_filings() {
        let full = rights(true, true, true, true, true, true, true, true);
        let in_school =
            BTreeMap::from([("M-in".to_string(), true), ("M-school".to_string(), true)]);
        let layout_of = |receipts: Value| {
            Layout::of(
                &[
                    mailbox("M-in", "Inbox", Some("inbox"), full.clone()),
                    mailbox("M-school", "School", None, full.clone()),
                    mailbox("M-receipts", "Receipts", None, receipts),
                ]
                .into_iter()
                .map(|m| serde_json::from_value::<Mailbox>(m).unwrap())
                .collect::<Vec<_>>(),
            )
        };

        let allowed = layout_of(full.clone());
        assert!(filing_allowed(&in_school, &allowed, Some("Receipts")));
        assert!(filing_allowed(&in_school, &allowed, None));

        // The target can't take items.
        let shut = rights(true, false, true, true, true, true, true, true);
        assert!(!filing_allowed(
            &in_school,
            &layout_of(shut),
            Some("Receipts")
        ));

        // The folder losing the message can't give it up.
        let locked = BTreeMap::from([("M-in".to_string(), true), ("M-school".to_string(), true)]);
        let hoarder = Layout::of(
            &[
                mailbox("M-in", "Inbox", Some("inbox"), full.clone()),
                mailbox("M-school", "School", None, {
                    let mut r = full.clone();
                    r["mayRemoveItems"] = json!(false);
                    r
                }),
                mailbox("M-receipts", "Receipts", None, full),
            ]
            .into_iter()
            .map(|m| serde_json::from_value::<Mailbox>(m).unwrap())
            .collect::<Vec<_>>(),
        );
        assert!(!filing_allowed(&locked, &hoarder, Some("Receipts")));
    }

    #[test]
    fn state_moves_follow_the_membership_table() {
        let layout = Layout::of(&mailboxes());
        assert_eq!(layout.archive.as_deref(), Some("M-archive"));
        let boxes = |ids: &[&str]| -> BTreeMap<String, bool> {
            ids.iter().map(|id| (id.to_string(), true)).collect()
        };
        let to = |ids: &[&str]| Move::To(boxes(ids));

        // Do and Watch leave the Inbox for their label; folders stay.
        let in_inbox = boxes(&["M-in", "M-school"]);
        assert_eq!(
            moved_mailboxes(&in_inbox, &layout, State::Do),
            to(&["M-do", "M-school"])
        );
        // Switching lanes swaps the label, and leaving Archive is part
        // of leaving Done.
        assert_eq!(
            moved_mailboxes(&boxes(&["M-do", "M-archive"]), &layout, State::Watch),
            to(&["M-watch"])
        );
        // Inbox puts it back, label off.
        assert_eq!(
            moved_mailboxes(&boxes(&["M-watch", "M-school"]), &layout, State::Inbox),
            to(&["M-in", "M-school"])
        );
        // Done takes it out of both and into Archive.
        assert_eq!(
            moved_mailboxes(
                &boxes(&["M-in", "M-watch", "M-school"]),
                &layout,
                State::Done
            ),
            to(&["M-archive", "M-school"])
        );

        // Already where the state says: nothing to push. Filed mail
        // with neither Inbox nor label already reads as Done, so Done
        // doesn't drag it into Archive; false memberships don't count.
        assert_eq!(
            moved_mailboxes(&in_inbox, &layout, State::Inbox),
            Move::Settled
        );
        assert_eq!(
            moved_mailboxes(&boxes(&["M-do"]), &layout, State::Do),
            Move::Settled
        );
        let filed_out =
            BTreeMap::from([("M-school".to_string(), true), ("M-in".to_string(), false)]);
        assert_eq!(
            moved_mailboxes(&filed_out, &layout, State::Done),
            Move::Settled
        );

        // No mailbox for the state: nowhere to land. This layout has no
        // Docket/Wait.
        assert_eq!(
            moved_mailboxes(&in_inbox, &layout, State::Wait),
            Move::Nowhere
        );
        let mut bare = Layout::of(&mailboxes());
        bare.archive = None;
        bare.inbox = None;
        assert_eq!(
            moved_mailboxes(&boxes(&["M-do"]), &bare, State::Done),
            Move::Nowhere
        );
        assert_eq!(
            moved_mailboxes(&boxes(&["M-do"]), &bare, State::Inbox),
            Move::Nowhere
        );

        // Full rights allow it.
        let next = boxes(&["M-archive", "M-school"]);
        let current = boxes(&["M-in", "M-school"]);
        assert!(move_allowed(&current, &next, &layout));

        let full = rights(true, true, true, true, true, true, true, true);

        // The Inbox hoarding or Archive shut stops it.
        let shut = |inbox: Value, archive: Value| {
            Layout::of(
                &[
                    mailbox("M-in", "Inbox", Some("inbox"), inbox),
                    mailbox("M-archive", "Archive", Some("archive"), archive),
                ]
                .into_iter()
                .map(|m| serde_json::from_value::<Mailbox>(m).unwrap())
                .collect::<Vec<_>>(),
            )
        };
        let mut hoard = full.clone();
        hoard["mayRemoveItems"] = json!(false);
        assert!(!move_allowed(&current, &next, &shut(hoard, full.clone())));
        let mut closed = full.clone();
        closed["mayAddItems"] = json!(false);
        assert!(!move_allowed(&current, &next, &shut(full.clone(), closed)));
    }

    #[test]
    fn a_destroy_reply_that_wont_parse_fails() {
        let err = parse_destroy_emails(json!({"destroyed": 3})).unwrap_err();
        assert!(err.to_string().contains("malformed Email/destroy"), "{err}");
    }

    #[test]
    fn set_emails_serializes_and_replies_parse() {
        let update = BTreeMap::from([(
            "E1".to_string(),
            EmailPatch {
                mailbox_ids: BTreeMap::from([("M1".to_string(), true)]),
            },
        )]);
        let args = serde_json::to_value(SetEmails {
            account_id: "a",
            update: &update,
        })
        .unwrap();
        assert_eq!(
            args,
            json!({"accountId": "a", "update": {"E1": {"mailboxIds": {"M1": true}}}})
        );

        let reply = serde_json::from_value::<SetEmailsReply>(json!({
            "accountId": "a", "oldState": "s1", "newState": "s2",
            "updated": {"E1": null},
            "notUpdated": {"E2": {"type": "tooManyRequests"}},
        }))
        .unwrap();
        assert!(reply.updated.contains_key("E1"));
        assert_eq!(reply.not_updated.len(), 1);

        // Both maps may be absent (RFC 8621 §5.3).
        let bare = serde_json::from_value::<SetEmailsReply>(json!({"accountId": "a"})).unwrap();
        assert!(bare.updated.is_empty());
        assert!(bare.not_updated.is_empty());
    }

    #[test]
    fn a_set_email_reply_that_wont_parse_fails() {
        let err = parse_set_emails(json!({"updated": 3})).unwrap_err();
        assert!(err.to_string().contains("malformed Email/set"), "{err}");
    }

    #[test]
    fn layout_sorts_mailboxes_into_roles_labels_and_folders() {
        let layout = Layout::of(&mailboxes());
        assert_eq!(layout.inbox.as_deref(), Some("M-in"));
        assert_eq!(layout.sent.as_deref(), Some("M-sent"));
        assert_eq!(
            layout.folder_names(),
            ["Receipts", "School"].map(String::from)
        );

        // A label nested under a `Docket` parent classifies the same as
        // a prefixed name.
        let nested = serde_json::from_value::<Mailbox>(mailbox(
            "M-parent",
            "Docket",
            None,
            rights(true, true, true, true, true, true, true, true),
        ))
        .unwrap();
        let mut all = mailboxes();
        all.insert(0, nested);
        assert_eq!(
            Layout::of(&all).folder_names(),
            ["Receipts", "School"].map(String::from)
        );
    }

    /// An Email with the given mailbox memberships.
    fn email(mailboxes: &[(&str, bool)]) -> Email {
        let json = json!({
            "id": "E1",
            "threadId": "T1",
            "messageId": ["msg-1@chislan.family"],
            "mailboxIds": BTreeMap::from_iter(
                mailboxes.iter().map(|(id, member)| (id.to_string(), json!(member)))
            ),
            "receivedAt": "2026-10-02T04:06:46Z",
            "sentAt": "2026-10-02T04:06:40Z",
            "from": [{"name": "Lincoln High School", "email": "office@lincolnhigh.org"}],
            "to": [{"email": "household@example.com"}],
            "cc": [],
            "bcc": [],
            "subject": "Field trip",
            "preview": "A schedule update…",
        });
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn an_emails_state_comes_from_its_docket_label() {
        let layout = Layout::of(&mailboxes());
        let plain = email(&[("M-in", true)]);
        assert_eq!(layout.state_of(&plain), State::Inbox);
        assert_eq!(layout.folder_of(&plain), None);
        assert!(!layout.is_sent(&plain));
        assert!(layout.is_inbox(&plain));

        let labeled = email(&[("M-in", true), ("M-watch", true)]);
        assert_eq!(layout.state_of(&labeled), State::Watch);

        let filed = email(&[("M-in", true), ("M-receipts", true)]);
        assert_eq!(layout.state_of(&filed), State::Inbox);
        assert_eq!(layout.folder_of(&filed).as_deref(), Some("Receipts"));

        // Not a member: filed out of Inbox, or a sent message.
        let gone = email(&[("M-receipts", true)]);
        assert!(!layout.is_inbox(&gone));
        // Neither Inbox nor label: Done. A lapsed membership is none.
        assert_eq!(layout.state_of(&gone), State::Done);
        assert_eq!(
            layout.state_of(&email(&[("M-in", true), ("M-do", false)])),
            State::Inbox
        );
        assert!(!layout.is_inbox(&email(&[("M-in", false)])));

        let mine = email(&[("M-sent", true)]);
        assert!(layout.is_sent(&mine));
        assert!(!layout.is_inbox(&mine));
    }

    #[test]
    fn the_body_falls_through_plain_html_then_preview() {
        let mut mail = email(&[("M-in", true)]);
        mail.text_body = Some(vec![BodyPart {
            part_id: "p1".into(),
            media_type: "text/plain; charset=utf-8".into(),
        }]);
        mail.body_values = Some(BTreeMap::from([(
            "p1".to_string(),
            BodyValue {
                value: "Buses return at 4:15.".into(),
            },
        )]));
        assert_eq!(body_of(&mail), "Buses return at 4:15.");

        // An empty plain part falls through to the HTML part instead of
        // stopping at the blank.
        mail.html_body = Some(vec![BodyPart {
            part_id: "h1".into(),
            media_type: "text/html".into(),
        }]);
        let values = mail.body_values.as_mut().unwrap();
        values.insert(
            "h1".into(),
            BodyValue {
                value: "<p>Buses return at 4:15.</p>".into(),
            },
        );
        values.insert(
            "p1".into(),
            BodyValue {
                value: "  \n".into(),
            },
        );
        assert_eq!(body_of(&mail), "Buses return at 4:15.\n");

        // HTML-only mail converts instead of showing the truncated
        // preview.
        mail.text_body = None;
        assert_eq!(body_of(&mail), "Buses return at 4:15.\n");

        // A plain part with no fetched body value also falls through.
        mail.text_body = Some(vec![
            BodyPart {
                part_id: "missing".into(),
                media_type: "text/plain".into(),
            },
            BodyPart {
                part_id: "p1".into(),
                media_type: "text/plain".into(),
            },
        ]);
        mail.body_values.as_mut().unwrap().insert(
            "p1".into(),
            BodyValue {
                value: "Buses return at 4:15.".into(),
            },
        );
        assert_eq!(body_of(&mail), "Buses return at 4:15.");

        // With no parts at all, the preview carries the body.
        mail.text_body = None;
        mail.html_body = None;
        assert_eq!(body_of(&mail), "A schedule update…");

        // A message with no text anywhere says so instead of rendering
        // blank.
        mail.preview = Some("  \n".into());
        assert_eq!(body_of(&mail), "(no body)");
    }

    #[test]
    fn a_query_page_that_wont_parse_fails() {
        let err = parse_query(json!({"ids": 3})).unwrap_err();
        assert!(err.to_string().contains("malformed Email/query"), "{err}");
    }

    #[test]
    fn changes_parse_with_their_paging_fields() {
        let args = json!({
            "accountId": "a", "oldState": "e0", "newState": "e1",
            "hasMoreChanges": true,
            "created": ["E1"], "updated": ["E2"], "destroyed": ["E3"],
        });
        let page = parse_changes("Email/changes", args).unwrap();
        assert!(page.changed());
        assert!(page.has_more_changes);
        assert_eq!(page.new_state, "e1");
        assert_eq!(page.created, ["E1".to_owned()]);
        assert_eq!(page.updated, ["E2".to_owned()]);
        assert_eq!(page.destroyed, ["E3".to_owned()]);

        let quiet = parse_changes(
            "Email/changes",
            json!({"accountId": "a", "oldState": "e1", "newState": "e1",
                   "hasMoreChanges": false,
                   "created": [], "updated": [], "destroyed": []}),
        )
        .unwrap();
        assert!(!quiet.changed());

        let err = parse_changes("Email/changes", json!({"created": 3})).unwrap_err();
        assert!(err.to_string().contains("malformed Email/changes"), "{err}");
    }

    #[test]
    fn a_stateful_list_carries_its_type_state() {
        let args = json!({"state": "m3", "list": [{"id": "M1", "name": "Inbox",
                       "role": "inbox", "myRights": rights(true, true, true, true,
                           true, true, true, true)}]});
        let (mailboxes, state) = parse_stateful_list::<Mailbox>("Mailbox/get", args).unwrap();
        assert_eq!(state, "m3");
        assert_eq!(mailboxes.len(), 1);

        let err = parse_stateful_list::<Mailbox>("Mailbox/get", json!({"list": []})).unwrap_err();
        assert!(err.to_string().contains("malformed Mailbox/get"), "{err}");
    }

    #[test]
    fn a_cannot_calculate_reply_is_its_own_error() {
        let reply = json!({
            "methodResponses": [[
                "error",
                {"type": "cannotCalculateChanges"},
                "0",
            ]],
        })
        .to_string();
        let err = parse_reply("Email/changes", &reply).unwrap_err();
        assert!(matches!(err, JmapError::CannotCalculateChanges { .. }));
        assert!(err.to_string().contains("cannot calculate"), "{err}");
    }

    #[test]
    fn incoming_mail_carries_its_derived_values() {
        let layout = Layout::of(&mailboxes());
        let users = [User::new("sam@example.com", "Sam")];

        let mut school = email(&[("M-in", true)]);
        school.subject = None;
        let mail = incoming(&school, &layout, &users).unwrap();
        assert_eq!(mail.subject, "(no subject)");
        assert_eq!(mail.at.to_string(), "2026-10-02T04:06:46");
        assert!(matches!(&mail.kind,
            IncomingKind::Received { from, addr, state, folder }
            if from == "Lincoln High School" && addr == "office@lincolnhigh.org"
                && *state == State::Inbox && folder.is_none()));

        // A sent reply attributes by From address; the shared identity
        // matches no login.
        let mut mine = email(&[("M-sent", true)]);
        mine.from = Some(vec![EmailAddress {
            name: Some("Sam".into()),
            email: "sam@example.com".into(),
        }]);
        assert!(matches!(&incoming(&mine, &layout, &users).unwrap().kind,
            IncomingKind::Sent { by, to }
            if by.as_deref() == Some("sam@example.com")
                && to == &["household@example.com".to_string()]));

        mine.from = Some(vec![EmailAddress {
            name: None,
            email: "household@example.com".into(),
        }]);
        assert!(matches!(&incoming(&mine, &layout, &users).unwrap().kind,
            IncomingKind::Sent { by, .. } if by.is_none()));

        // Nothing to show for a senderless oddity.
        let mut odd = email(&[("M-in", true)]);
        odd.from = None;
        assert!(incoming(&odd, &layout, &users).is_none());
    }

    #[test]
    fn the_fastmail_client_builds() {
        assert!(Client::fastmail().is_ok());
    }

    #[test]
    fn an_empty_token_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.token");
        fs_err::write(&path, " \n").unwrap();
        let path = Utf8PathBuf::try_from(path).unwrap();
        let err = read_token(&path).unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
    }

    #[test]
    fn a_credential_describes_what_it_expects() {
        // The kdl paths always hand the visitors a map, so serde never
        // has to name the expectation; a scalar makes it.
        let err = serde_json::from_str::<Credential>("5").unwrap_err();
        assert!(err.to_string().contains("a credential node"), "{err}");
        let err = serde_json::from_str::<Credentials>("5").unwrap_err();
        assert!(err.to_string().contains("credential nodes"), "{err}");
    }

    /// Mirrors how `Config` declares the field, so the kdl paths (lone
    /// node and group) both get exercised.
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Doc {
        #[serde(default, rename = "credential")]
        credentials: Credentials,
    }

    fn parse_credentials(source: &str) -> Result<Doc, kdl::de::Error> {
        kdl::de::from_str(source)
    }

    #[test]
    fn credentials_parse_in_document_order() {
        let doc = parse_credentials(
            "credential \"household\" token-file=\"/run/credentials/docket/household\"\n\
             credential \"eli\" token-file=\"/run/credentials/docket/eli\"",
        )
        .unwrap();
        let names: Vec<_> = doc.credentials.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["household", "eli"]);
        assert_eq!(
            doc.credentials.first().unwrap().token_file,
            "/run/credentials/docket/household"
        );
    }

    #[test]
    fn a_lone_credential_parses() {
        let doc = parse_credentials(r#"credential "household" token-file="/tok""#).unwrap();
        let credential = doc.credentials.first().unwrap();
        assert_eq!(credential.name, "household");
        assert_eq!(credential.token_file, "/tok");
    }

    #[test]
    fn no_credential_nodes_is_empty() {
        assert!(parse_credentials("").unwrap().credentials.is_empty());
        assert!(
            parse_credentials("// none\n")
                .unwrap()
                .credentials
                .is_empty()
        );
    }

    #[test]
    fn malformed_credential_nodes_fail_with_pointers() {
        for (source, message) in [
            (
                r#"credential "household""#,
                "a credential needs a token-file property",
            ),
            (
                r#"credential token-file="/tok""#,
                "a credential is named by its argument",
            ),
            (
                r#"credential "household" "x" token-file="/tok""#,
                "a credential takes one argument",
            ),
            (
                r#"credential "household" file="/tok""#,
                "unexpected property `file`",
            ),
        ] {
            let err = parse_credentials(source).unwrap_err();
            assert!(err.to_string().contains(message), "{source}: {err}");
        }
    }
}
