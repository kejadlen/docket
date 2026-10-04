//! The JMAP side of Docket: one session per credential (ADR 1), with
//! rights derived from what the server permits rather than config. Token
//! scope surfaces in the session as capabilities and `isReadOnly`;
//! per-mailbox `myRights` report mailbox ACLs, so they gate per-mailbox
//! actions like filing (task rn), never account-wide scope.

use std::collections::BTreeMap;
use std::fmt;
use std::io::ErrorKind;
use std::time::Duration;

use camino::{Utf8Path, Utf8PathBuf};
use serde::Deserialize;
use serde::de;
use serde::de::value::MapAccessDeserializer;
use serde_json::Value;

use crate::Error;
use crate::model::Account;
use crate::store::Store;

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

    #[error("credential {credential:?} offers no mail account in its session")]
    NoMailAccount { credential: String },
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
struct MailboxGet {
    list: Vec<Mailbox>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(rename = "type")]
    kind: String,
    description: Option<String>,
}

fn parse_mailboxes(json: &str) -> Result<Vec<Mailbox>, JmapError> {
    const WHAT: &str = "Mailbox/get reply";
    let reply: Reply =
        serde_json::from_str(json).map_err(|source| JmapError::Malformed { what: WHAT, source })?;
    let detail = |text: String| JmapError::Reply {
        method: "Mailbox/get",
        detail: text,
    };
    match reply.method_responses.as_slice() {
        [(method, args, _)] => match method.as_str() {
            "Mailbox/get" => {
                let got: MailboxGet = serde_json::from_value(args.clone())
                    .map_err(|source| JmapError::Malformed { what: WHAT, source })?;
                Ok(got.list)
            }
            "error" => {
                let err: ApiError = serde_json::from_value(args.clone())
                    .map_err(|source| JmapError::Malformed { what: WHAT, source })?;
                Err(match err.description {
                    Some(description) => detail(format!("{}: {description}", err.kind)),
                    None => detail(err.kind),
                })
            }
            other => Err(detail(format!("unexpected response {other:?}"))),
        },
        [] => Err(detail("the reply had no methodResponses".into())),
        _ => Err(detail("the reply had more than one methodResponse".into())),
    }
}

/// Opens sessions: one per credential, against Fastmail unless a test
/// points the session URL at a stub.
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
        let body = self
            .http
            .get(&self.session_url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Session::parse(&body)
    }

    /// Every mailbox of the account, with the rights the server reports
    /// for it.
    async fn mailboxes(
        &self,
        session: &Session,
        token: &str,
        account_id: &str,
    ) -> Result<Vec<Mailbox>, JmapError> {
        let request = serde_json::json!({
            "using": [CORE, MAIL],
            "methodCalls": [[
                "Mailbox/get",
                {"accountId": account_id, "ids": null,
                 "properties": ["id", "name", "role", "myRights"]},
                "0",
            ]],
        });
        let body = self
            .http
            .post(&session.api_url)
            .bearer_auth(token)
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        parse_mailboxes(&body)
    }

    /// Opens the credential's session and records its account in the
    /// store, with the rights the server reports: [`Import::account`] is
    /// the seam, and its upsert makes this the refresh path too — rights
    /// are re-read every time a session opens.
    ///
    /// [`Import::account`]: crate::store::Import::account
    pub async fn sync_account(&self, credential: &Credential, store: &Store) -> Result<(), Error> {
        let token = read_token(&credential.token_file)?;
        let session = self.session(&token).await?;
        let (id, account) = session.mail_account(&credential.name)?;
        let rights = account.rights();
        let mailboxes = self.mailboxes(&session, &token, id).await?;
        let mailbox_count = mailboxes.len();
        tracing::info!(
            credential = %credential.name,
            account = %id,
            read_only = rights.read_only,
            send = rights.send,
            mailboxes = mailbox_count,
            "session opened",
        );
        store.import(|tx| {
            tx.account(&Account {
                slug: credential.name.clone(),
                name: account.name.clone(),
                // Fastmail accountIds are the login's address (spike
                // 2026-10-03).
                address: id.to_owned(),
                read_only: rights.read_only,
            })
        })
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
        let mailboxes = parse_mailboxes(&reply).unwrap();
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
    fn an_error_reply_fails_the_mailbox_fetch() {
        let reply = json!({
            "methodResponses": [[
                "error",
                {"type": "serverFail", "description": "boom"},
                "0",
            ]],
        })
        .to_string();
        let err = parse_mailboxes(&reply).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Mailbox/get"), "{text}");
        assert!(text.contains("serverFail: boom"), "{text}");

        let undescribed = json!({
            "methodResponses": [["error", {"type": "serverFail"}, "0"]],
        })
        .to_string();
        assert!(
            parse_mailboxes(&undescribed)
                .unwrap_err()
                .to_string()
                .contains("serverFail")
        );
    }

    #[test]
    fn a_reply_that_is_not_the_method_asked_for_fails() {
        let wrong = json!({"methodResponses": [["Email/get", {"list": []}, "0"]]}).to_string();
        assert!(
            parse_mailboxes(&wrong)
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
            parse_mailboxes(&doubled)
                .unwrap_err()
                .to_string()
                .contains("more than one methodResponse")
        );
    }

    #[test]
    fn malformed_replies_fail_to_parse() {
        assert!(Session::parse("{").is_err());
        assert!(parse_mailboxes("[]").is_err());
        assert!(parse_mailboxes(r#"{"methodResponses": []}"#).is_err());
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
