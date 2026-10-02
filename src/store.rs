//! Docket's data in SQLite: a cache of the mail it tracks (from fixtures in
//! dev until JMAP import exists), and the state, assignees, reads, comments,
//! and history only Docket keeps. Every read is a query returning owned
//! values. Undo and toasts belong to the browsing session and stay in
//! memory.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard, PoisonError};

use camino::Utf8Path;
use jiff::civil::DateTime;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{Connection, OptionalExtension as _, Params, Row, ToSql, Transaction, params};

use crate::Error;
use crate::model::{
    Account, Comment, CommentId, Event, Kind, Message, MessageId, State, Thread, ThreadId, User,
    Values,
};

/// Applied in order; `PRAGMA user_version` counts how many have run.
const MIGRATIONS: &[&str] = &[include_str!("../migrations/0001_init.sql")];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    State(State),
    Folder(Option<String>),
    ToggleAssignee(String),
}

/// A one-shot notice shown to a user after an edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    pub text: String,
    pub undoable: bool,
}

/// An entry in a thread's chain, in time order.
#[derive(Debug, Clone)]
pub enum Item {
    Message(Message),
    Comment(Comment),
}

/// Which messages a list asks for.
#[derive(Debug, Clone, Copy)]
pub enum Filter<'a> {
    /// Received messages in a state.
    State(State),
    /// Received messages in a state that are the user's: assigned to them,
    /// or unassigned in Inbox.
    ForMe { user: &'a str, state: State },
    /// Messages whose subject, sender, or body contains the text. Case is
    /// ignored for ASCII letters only, as SQLite's LIKE does.
    Search(&'a str),
}

/// One connection behind a mutex: two people's clicks never contend long
/// enough to need a pool, and no lock is held across an await.
#[derive(Debug)]
pub struct Store {
    /// The clock fixtures are written against, so ages stay stable.
    pub now: DateTime,
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    conn: Connection,
    undo: BTreeMap<String, (MessageId, Values)>,
    flash: BTreeMap<String, Flash>,
}

impl Store {
    /// Opens (creating if needed) the database file and brings its schema
    /// up to date.
    pub fn open(path: &Utf8Path, now: DateTime) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn, now)
    }

    /// A database that lasts as long as the store, for tests.
    pub fn open_in_memory(now: DateTime) -> Result<Self, Error> {
        Self::init(Connection::open_in_memory()?, now)
    }

    fn init(mut conn: Connection, now: DateTime) -> Result<Self, Error> {
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        Ok(Self {
            now,
            inner: Mutex::new(Inner {
                conn,
                undo: BTreeMap::new(),
                flash: BTreeMap::new(),
            }),
        })
    }

    /// True before anyone has been added: a fresh database.
    pub fn is_empty(&self) -> Result<bool, Error> {
        let inner = self.lock();
        let empty = inner
            .conn
            .query_row("SELECT NOT EXISTS (SELECT 1 FROM users)", [], |r| r.get(0))?;
        Ok(empty)
    }

    /// Writes mail and people in one transaction: fixtures now, JMAP later.
    pub fn import(&self, f: impl FnOnce(&Import<'_>) -> Result<(), Error>) -> Result<(), Error> {
        let mut inner = self.lock();
        let import = Import {
            tx: inner.conn.transaction()?,
        };
        f(&import)?;
        import.tx.commit()?;
        Ok(())
    }

    pub fn users(&self) -> Result<Vec<User>, Error> {
        let inner = self.lock();
        let mut stmt = inner
            .conn
            .prepare_cached("SELECT slug, name, login FROM users ORDER BY rowid")?;
        let users = stmt
            .query_map([], user_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(users)
    }

    pub fn user(&self, slug: &str) -> Result<Option<User>, Error> {
        load_user(&self.lock().conn, slug)
    }

    pub fn user_by_login(&self, login: &str) -> Result<Option<User>, Error> {
        let user = self
            .lock()
            .conn
            .query_row(
                "SELECT slug, name, login FROM users WHERE login = ?1",
                [login],
                user_from_row,
            )
            .optional()?;
        Ok(user)
    }

    pub fn folders(&self) -> Result<Vec<String>, Error> {
        let inner = self.lock();
        let mut stmt = inner
            .conn
            .prepare_cached("SELECT name FROM folders ORDER BY rowid")?;
        let folders = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(folders)
    }

    pub fn thread(&self, id: ThreadId) -> Result<Option<Thread>, Error> {
        let thread = self
            .lock()
            .conn
            .query_row(
                "SELECT id, account, subject FROM threads WHERE id = ?1",
                [id],
                |r| {
                    Ok(Thread {
                        id: r.get(0)?,
                        account: r.get(1)?,
                        subject: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(thread)
    }

    pub fn thread_account(&self, thread: ThreadId) -> Result<Option<Account>, Error> {
        load_thread_account(&self.lock().conn, thread)
    }

    pub fn message(&self, id: MessageId) -> Result<Option<Message>, Error> {
        load_message(&self.lock().conn, id)
    }

    /// The thread's messages, oldest first.
    pub fn thread_messages(&self, thread: ThreadId) -> Result<Vec<Message>, Error> {
        load_messages(&self.lock().conn, "m.thread = ?1", [thread])
    }

    /// Messages matching the filter, oldest first.
    pub fn messages(&self, filter: Filter<'_>) -> Result<Vec<Message>, Error> {
        let inner = self.lock();
        let conn = &inner.conn;
        match filter {
            Filter::State(state) => {
                load_messages(conn, "m.kind = 'received' AND m.state = ?1", [state])
            }
            Filter::ForMe { user, state } => load_messages(
                conn,
                "m.kind = 'received' AND m.state = ?1 AND (
                    EXISTS (SELECT 1 FROM assignees a WHERE a.message = m.id AND a.user = ?2)
                    OR (?1 = 'inbox' AND NOT EXISTS (SELECT 1 FROM assignees a WHERE a.message = m.id))
                )",
                params![state, user],
            ),
            Filter::Search(text) => {
                let pattern = format!("%{}%", escape_like(text));
                load_messages(
                    conn,
                    r"t.subject LIKE ?1 ESCAPE '\' OR m.from_name LIKE ?1 ESCAPE '\'
                        OR u.name LIKE ?1 ESCAPE '\' OR m.body LIKE ?1 ESCAPE '\'",
                    [pattern],
                )
            }
        }
    }

    /// The thread's messages and comments in time order. A message sorts
    /// before a comment made at the same moment.
    pub fn timeline(&self, thread: ThreadId) -> Result<Vec<Item>, Error> {
        let inner = self.lock();
        let messages = load_messages(&inner.conn, "m.thread = ?1", [thread])?;
        let mut stmt = inner.conn.prepare_cached(
            "SELECT id, thread, author, at, text FROM comments WHERE thread = ?1 ORDER BY at, id",
        )?;
        let comments = stmt
            .query_map([thread], |r| {
                Ok(Comment {
                    id: r.get(0)?,
                    thread: r.get(1)?,
                    author: r.get(2)?,
                    at: r.get(3)?,
                    text: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut items: Vec<_> = messages
            .into_iter()
            .map(Item::Message)
            .chain(comments.into_iter().map(Item::Comment))
            .collect();
        items.sort_by_key(|item| match item {
            Item::Message(m) => m.at,
            Item::Comment(c) => c.at,
        });
        Ok(items)
    }

    /// Received messages the user hasn't opened.
    pub fn unread(&self, user: &str) -> Result<BTreeSet<MessageId>, Error> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT id FROM messages WHERE kind = 'received'
                AND id NOT IN (SELECT message FROM reads WHERE user = ?1)",
        )?;
        let ids = stmt
            .query_map([user], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    pub fn mark_read(&self, user: &str, msg: MessageId) -> Result<(), Error> {
        insert_read(&self.lock().conn, user, msg)
    }

    /// Every change made to the message's values, oldest first.
    pub fn history(&self, message: MessageId) -> Result<Vec<Event>, Error> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT message, user, at, event FROM history WHERE message = ?1 ORDER BY id",
        )?;
        let events = stmt
            .query_map([message], |r| {
                Ok(Event {
                    message: r.get(0)?,
                    user: r.get(1)?,
                    at: r.get(2)?,
                    text: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(events)
    }

    pub fn edit(&self, user: &str, id: MessageId, change: Change) -> Result<(), Error> {
        let mut inner = self.lock();
        let tx = inner.conn.transaction()?;
        let msg = load_message(&tx, id)?.ok_or(Error::NotFound("message"))?;
        let read_only = load_thread_account(&tx, msg.thread)?.is_some_and(|a| a.read_only);
        let Kind::Received { values, .. } = msg.kind else {
            return Err(Error::BadRequest("sent messages have no values"));
        };
        let prev = values;
        let mut next = prev.clone();

        let text = match change {
            Change::State(state) => {
                next.state = state;
                format!("Moved to {state}")
            }
            Change::Folder(folder) => {
                if read_only {
                    return Err(Error::Forbidden("this account is read-only"));
                }
                match folder {
                    Some(f) if !folder_exists(&tx, &f)? => {
                        return Err(Error::NotFound("folder"));
                    }
                    Some(f) => {
                        let text = format!("Filed to {f}");
                        next.folder = Some(f);
                        text
                    }
                    None => {
                        next.folder = None;
                        "Removed from folder".to_owned()
                    }
                }
            }
            Change::ToggleAssignee(slug) => {
                let name = load_user(&tx, &slug)?.ok_or(Error::NotFound("user"))?.name;
                if next.assignees.remove(&slug) {
                    format!("Unassigned {name}")
                } else {
                    next.assignees.insert(slug);
                    format!("Assigned {name}")
                }
            }
        };

        write_values(&tx, id, &next)?;
        insert_event(&tx, id, user, self.now, &text)?;
        tx.commit()?;
        inner.undo.insert(user.to_owned(), (id, prev));
        inner.flash.insert(
            user.to_owned(),
            Flash {
                text,
                undoable: true,
            },
        );
        Ok(())
    }

    pub fn undo(&self, user: &str) -> Result<(), Error> {
        let mut inner = self.lock();
        let (id, prev) = inner
            .undo
            .get(user)
            .cloned()
            .ok_or(Error::BadRequest("nothing to undo"))?;
        let tx = inner.conn.transaction()?;
        write_values(&tx, id, &prev)?;
        insert_event(&tx, id, user, self.now, "Undone")?;
        tx.commit()?;
        inner.undo.remove(user);
        inner.flash.insert(
            user.to_owned(),
            Flash {
                text: "Undone".to_owned(),
                undoable: false,
            },
        );
        Ok(())
    }

    pub fn take_flash(&self, user: &str) -> Option<Flash> {
        self.lock().flash.remove(user)
    }

    pub fn add_comment(&self, user: &str, thread: ThreadId, text: &str) -> Result<(), Error> {
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::BadRequest("comment is empty"));
        }
        let inner = self.lock();
        let exists: bool = inner.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM threads WHERE id = ?1)",
            [thread],
            |r| r.get(0),
        )?;
        if !exists {
            return Err(Error::NotFound("thread"));
        }
        insert_comment(&inner.conn, None, thread, user, self.now, text)?;
        Ok(())
    }

    /// A panic mid-request can't leave SQLite half-written (transactions
    /// roll back on drop), so a poisoned lock is safe to keep using.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Writes inside one transaction, from [`Store::import`].
pub struct Import<'a> {
    tx: Transaction<'a>,
}

impl Import<'_> {
    pub fn user(&self, user: &User) -> Result<(), Error> {
        self.tx.execute(
            "INSERT INTO users (slug, name, login) VALUES (?1, ?2, ?3)",
            params![user.slug, user.name, user.login],
        )?;
        Ok(())
    }

    pub fn account(&self, account: &Account) -> Result<(), Error> {
        self.tx.execute(
            "INSERT INTO accounts (slug, name, address, read_only) VALUES (?1, ?2, ?3, ?4)",
            params![
                account.slug,
                account.name,
                account.address,
                account.read_only
            ],
        )?;
        Ok(())
    }

    pub fn folder(&self, name: &str) -> Result<(), Error> {
        self.tx
            .execute("INSERT INTO folders (name) VALUES (?1)", [name])?;
        Ok(())
    }

    pub fn thread(&self, thread: &Thread) -> Result<(), Error> {
        self.tx.execute(
            "INSERT INTO threads (id, account, subject) VALUES (?1, ?2, ?3)",
            params![thread.id, thread.account, thread.subject],
        )?;
        Ok(())
    }

    /// The message lands in its thread's account.
    pub fn message(&self, m: &Message) -> Result<(), Error> {
        let (kind, from_name, from_addr, state, folder, sent_by, sent_to) = match &m.kind {
            Kind::Received { from, addr, values } => (
                "received",
                Some(from),
                Some(addr),
                Some(values.state),
                values.folder.as_ref(),
                None,
                None,
            ),
            Kind::Sent { by, to } => ("sent", None, None, None, None, Some(by), Some(json(to)?)),
        };
        let inserted = self.tx.execute(
            "INSERT INTO messages (id, account, message_id, thread, at, cc, bcc, body, kind,
                from_name, from_addr, state, folder, sent_by, sent_to)
             SELECT ?1, account, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14
             FROM threads WHERE id = ?3",
            params![
                m.id,
                m.message_id,
                m.thread,
                m.at,
                json(&m.cc)?,
                json(&m.bcc)?,
                m.body,
                kind,
                from_name,
                from_addr,
                state,
                folder,
                sent_by,
                sent_to,
            ],
        )?;
        if inserted == 0 {
            return Err(Error::NotFound("thread"));
        }
        if let Some(values) = m.values() {
            write_values(&self.tx, m.id, values)?;
        }
        Ok(())
    }

    pub fn comment(&self, c: &Comment) -> Result<(), Error> {
        insert_comment(&self.tx, Some(c.id), c.thread, &c.author, c.at, &c.text)
    }

    pub fn read(&self, user: &str, msg: MessageId) -> Result<(), Error> {
        insert_read(&self.tx, user, msg)
    }
}

impl ToSql for State {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(self.slug().into())
    }
}

impl FromSql for State {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let slug = value.as_str()?;
        State::from_slug(slug).ok_or_else(|| FromSqlError::Other(format!("no state {slug}").into()))
    }
}

fn migrate(conn: &mut Connection) -> Result<(), Error> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    for (n, sql) in (1_i64..).zip(MIGRATIONS) {
        if n <= version {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", n)?;
        tx.commit()?;
    }
    Ok(())
}

fn user_from_row(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        slug: r.get(0)?,
        name: r.get(1)?,
        login: r.get(2)?,
    })
}

fn load_user(conn: &Connection, slug: &str) -> Result<Option<User>, Error> {
    let user = conn
        .query_row(
            "SELECT slug, name, login FROM users WHERE slug = ?1",
            [slug],
            user_from_row,
        )
        .optional()?;
    Ok(user)
}

fn load_thread_account(conn: &Connection, thread: ThreadId) -> Result<Option<Account>, Error> {
    let account = conn
        .query_row(
            "SELECT a.slug, a.name, a.address, a.read_only
             FROM accounts a JOIN threads t ON t.account = a.slug WHERE t.id = ?1",
            [thread],
            |r| {
                Ok(Account {
                    slug: r.get(0)?,
                    name: r.get(1)?,
                    address: r.get(2)?,
                    read_only: r.get(3)?,
                })
            },
        )
        .optional()?;
    Ok(account)
}

fn folder_exists(conn: &Connection, name: &str) -> Result<bool, Error> {
    let exists = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM folders WHERE name = ?1)",
        [name],
        |r| r.get(0),
    )?;
    Ok(exists)
}

fn load_message(conn: &Connection, id: MessageId) -> Result<Option<Message>, Error> {
    Ok(load_messages(conn, "m.id = ?1", [id])?.into_iter().next())
}

/// Messages matching a WHERE clause over `m` (messages), `t` (their
/// thread), and `u` (the user who sent them, if one of us did).
fn load_messages(
    conn: &Connection,
    filter: &str,
    params: impl Params,
) -> Result<Vec<Message>, Error> {
    let sql = format!(
        "SELECT m.id, m.message_id, m.thread, m.at, m.cc, m.bcc, m.body, m.kind,
            m.from_name, m.from_addr, m.state, m.folder, m.sent_by, m.sent_to,
            (SELECT json_group_array(user)
                FROM (SELECT user FROM assignees WHERE message = m.id ORDER BY user))
         FROM messages m
         JOIN threads t ON t.id = m.thread
         LEFT JOIN users u ON u.slug = m.sent_by
         WHERE {filter}
         ORDER BY m.at, m.id"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let messages = stmt
        .query_map(params, message_from_row)?
        .collect::<Result<_, _>>()?;
    Ok(messages)
}

fn message_from_row(r: &Row<'_>) -> rusqlite::Result<Message> {
    let kind = if r.get::<_, String>(7)? == "sent" {
        Kind::Sent {
            by: r.get(12)?,
            to: parse_json(r, 13)?,
        }
    } else {
        Kind::Received {
            from: r.get(8)?,
            addr: r.get(9)?,
            values: Values {
                state: r.get(10)?,
                folder: r.get(11)?,
                assignees: parse_json(r, 14)?,
            },
        }
    };
    Ok(Message {
        id: r.get(0)?,
        message_id: r.get(1)?,
        thread: r.get(2)?,
        at: r.get(3)?,
        cc: parse_json(r, 4)?,
        bcc: parse_json(r, 5)?,
        body: r.get(6)?,
        kind,
    })
}

fn write_values(conn: &Connection, id: MessageId, values: &Values) -> Result<(), Error> {
    conn.execute(
        "UPDATE messages SET state = ?2, folder = ?3 WHERE id = ?1",
        params![id, values.state, values.folder],
    )?;
    conn.execute("DELETE FROM assignees WHERE message = ?1", [id])?;
    for user in &values.assignees {
        conn.execute(
            "INSERT INTO assignees (message, user) VALUES (?1, ?2)",
            params![id, user],
        )?;
    }
    Ok(())
}

fn insert_event(
    conn: &Connection,
    message: MessageId,
    user: &str,
    at: DateTime,
    text: &str,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO history (message, user, at, event) VALUES (?1, ?2, ?3, ?4)",
        params![message, user, at, text],
    )?;
    Ok(())
}

fn insert_comment(
    conn: &Connection,
    id: Option<CommentId>,
    thread: ThreadId,
    author: &str,
    at: DateTime,
    text: &str,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO comments (id, thread, author, at, text) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, thread, author, at, text],
    )?;
    Ok(())
}

fn insert_read(conn: &Connection, user: &str, msg: MessageId) -> Result<(), Error> {
    conn.execute(
        "INSERT OR IGNORE INTO reads (user, message) VALUES (?1, ?2)",
        params![user, msg],
    )?;
    Ok(())
}

/// Lists of names and addresses are stored as JSON arrays.
fn json<T: serde::Serialize>(value: &T) -> rusqlite::Result<String> {
    serde_json::to_string(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))
}

fn parse_json<T: serde::de::DeserializeOwned>(r: &Row<'_>, idx: usize) -> rusqlite::Result<T> {
    let text: String = r.get(idx)?;
    serde_json::from_str(&text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
    })
}

/// Makes `%` and `_` in search text match themselves.
fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn values(store: &Store, id: MessageId) -> Values {
        store
            .message(id)
            .unwrap()
            .unwrap()
            .values()
            .unwrap()
            .clone()
    }

    #[test]
    fn edits_record_undo_flash_and_history() {
        let store = fixtures::store().unwrap();
        store.edit("sam", 4, Change::State(State::Do)).unwrap();
        assert_eq!(values(&store, 4).state, State::Do);
        assert_eq!(
            store.take_flash("sam"),
            Some(Flash {
                text: "Moved to Do".into(),
                undoable: true
            })
        );
        assert_eq!(store.take_flash("sam"), None);

        store.undo("sam").unwrap();
        assert_eq!(values(&store, 4).state, State::Inbox);
        assert_eq!(store.take_flash("sam").unwrap().text, "Undone");
        assert!(matches!(store.undo("sam"), Err(Error::BadRequest(_))));

        let history: Vec<_> = store
            .history(4)
            .unwrap()
            .into_iter()
            .map(|e| (e.user, e.at, e.text))
            .collect();
        assert_eq!(
            history,
            [
                ("sam".into(), store.now, "Moved to Do".into()),
                ("sam".into(), store.now, "Undone".into()),
            ]
        );
    }

    #[test]
    fn folders() {
        let store = fixtures::store().unwrap();
        store
            .edit("sam", 4, Change::Folder(Some("Finance".into())))
            .unwrap();
        assert_eq!(values(&store, 4).folder.as_deref(), Some("Finance"));
        store.edit("sam", 4, Change::Folder(None)).unwrap();
        assert_eq!(values(&store, 4).folder, None);
        assert_eq!(store.take_flash("sam").unwrap().text, "Removed from folder");
        assert!(matches!(
            store.edit("sam", 4, Change::Folder(Some("Nope".into()))),
            Err(Error::NotFound("folder"))
        ));
        assert_eq!(
            store.folders().unwrap(),
            ["Medical", "School", "House", "Finance"]
        );
    }

    #[test]
    fn read_only_accounts_cannot_file() {
        let store = fixtures::store().unwrap();
        let eli = fixtures::ELI_PRACTICE;
        assert!(matches!(
            store.edit("sam", eli, Change::Folder(None)),
            Err(Error::Forbidden(_))
        ));
        store.edit("sam", eli, Change::State(State::Done)).unwrap();
    }

    #[test]
    fn assignees_toggle() {
        let store = fixtures::store().unwrap();
        store
            .edit("alex", 4, Change::ToggleAssignee("alex".into()))
            .unwrap();
        assert!(values(&store, 4).assignees.contains("alex"));
        assert_eq!(store.take_flash("alex").unwrap().text, "Assigned Alex");
        store
            .edit("alex", 4, Change::ToggleAssignee("alex".into()))
            .unwrap();
        assert!(values(&store, 4).assignees.is_empty());
        assert_eq!(store.take_flash("alex").unwrap().text, "Unassigned Alex");
        assert!(matches!(
            store.edit("alex", 4, Change::ToggleAssignee("eli".into())),
            Err(Error::NotFound("user"))
        ));
    }

    #[test]
    fn bad_edits() {
        let store = fixtures::store().unwrap();
        assert!(matches!(
            store.edit("sam", 999, Change::State(State::Do)),
            Err(Error::NotFound("message"))
        ));
        assert!(matches!(
            store.edit("sam", 2, Change::State(State::Do)),
            Err(Error::BadRequest(_))
        ));
    }

    #[test]
    fn comments() {
        let store = fixtures::store().unwrap();
        let before = store.timeline(1).unwrap().len();
        store.add_comment("sam", 1, "  Called them.  ").unwrap();
        let timeline = store.timeline(1).unwrap();
        assert_eq!(timeline.len(), before + 1);
        assert!(matches!(timeline.last(), Some(Item::Comment(c)) if c.text == "Called them."));
        assert!(matches!(
            store.add_comment("sam", 1, "   "),
            Err(Error::BadRequest(_))
        ));
        assert!(matches!(
            store.add_comment("sam", 999, "hi"),
            Err(Error::NotFound("thread"))
        ));
    }

    #[test]
    fn timeline_interleaves_comments_by_time() {
        let store = fixtures::store().unwrap();
        let order: Vec<_> = store
            .timeline(1)
            .unwrap()
            .into_iter()
            .map(|item| match item {
                Item::Message(m) => format!("m{}", m.id),
                Item::Comment(c) => format!("c{}", c.id),
            })
            .collect();
        assert_eq!(order, ["m1", "c1", "m2", "m3", "m4"]);
    }

    #[test]
    fn read_tracking() {
        let store = fixtures::store().unwrap();
        assert!(store.unread("alex").unwrap().contains(&fixtures::WATER));
        store.mark_read("alex", fixtures::WATER).unwrap();
        store.mark_read("alex", fixtures::WATER).unwrap();
        assert!(!store.unread("alex").unwrap().contains(&fixtures::WATER));
        assert!(store.unread("sam").unwrap().contains(&fixtures::WATER));
        // Sent messages are never unread.
        assert!(!store.unread("sam").unwrap().contains(&2));
    }

    #[test]
    fn messages_round_trip() {
        let store = fixtures::store().unwrap();
        let sent = store.message(2).unwrap().unwrap();
        assert_eq!(sent.message_id, "2@fixtures.docket.invalid");
        assert_eq!(sent.cc, ["Alex"]);
        assert_eq!(sent.bcc, ["Pat Lee"]);
        assert!(
            matches!(&sent.kind, Kind::Sent { by, to } if by == "sam" && to == &["Northwind Roofing"])
        );
        let got = store.message(5).unwrap().unwrap();
        assert_eq!(got.at, jiff::civil::date(2026, 9, 30).at(15, 30, 0, 0));
        assert_eq!(
            got.values(),
            Some(&Values {
                state: State::Do,
                folder: Some("School".into()),
                assignees: BTreeSet::from(["sam".to_owned()]),
            })
        );
        assert!(store.message(999).unwrap().is_none());
    }

    #[test]
    fn search_treats_wildcards_literally() {
        let store = fixtures::store().unwrap();
        assert!(store.messages(Filter::Search("%")).unwrap().is_empty());
        assert!(store.messages(Filter::Search("_")).unwrap().is_empty());
        assert_eq!(store.messages(Filter::Search("$1,840")).unwrap().len(), 1);
        assert_eq!(escape_like(r"a%b_c\d"), r"a\%b\_c\\d");
    }

    #[test]
    fn lookups() {
        let store = fixtures::store().unwrap();
        let slugs: Vec<_> = store.users().unwrap().into_iter().map(|u| u.slug).collect();
        assert_eq!(slugs, ["alex", "sam"]);
        assert_eq!(store.user("sam").unwrap().unwrap().name, "Sam");
        assert!(store.user("pat").unwrap().is_none());
        assert_eq!(
            store
                .user_by_login("alex@example.com")
                .unwrap()
                .unwrap()
                .slug,
            "alex"
        );
        assert!(store.user_by_login("nobody").unwrap().is_none());
        assert_eq!(
            store.thread(1).unwrap().unwrap().subject,
            "Gutter repair estimate"
        );
        assert!(store.thread(999).unwrap().is_none());
        assert!(store.thread_account(11).unwrap().unwrap().read_only);
        assert!(store.thread_account(999).unwrap().is_none());
    }

    #[test]
    fn importing_into_a_missing_thread_fails() {
        let store = fixtures::store().unwrap();
        let mut m = store.message(4).unwrap().unwrap();
        m.id = 100;
        m.message_id = "new@example.com".into();
        m.thread = 999;
        let result = store.import(|tx| tx.message(&m));
        assert!(matches!(result, Err(Error::NotFound("thread"))));
        assert!(store.message(100).unwrap().is_none());
    }

    #[test]
    fn edits_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::try_from(dir.path().join("docket.db")).unwrap();

        let store = Store::open(&path, fixtures::now()).unwrap();
        assert!(store.is_empty().unwrap());
        fixtures::seed(&store).unwrap();
        assert!(!store.is_empty().unwrap());
        store.edit("sam", 4, Change::State(State::Wait)).unwrap();
        store
            .edit("sam", 4, Change::ToggleAssignee("alex".into()))
            .unwrap();
        store.add_comment("sam", 1, "Called them.").unwrap();
        store.mark_read("sam", fixtures::WATER).unwrap();
        drop(store);

        let store = Store::open(&path, fixtures::now()).unwrap();
        assert!(!store.is_empty().unwrap());
        let v = values(&store, 4);
        assert_eq!(v.state, State::Wait);
        assert_eq!(v.assignees, BTreeSet::from(["alex".to_owned()]));
        assert!(matches!(
            store.timeline(1).unwrap().last(),
            Some(Item::Comment(c)) if c.text == "Called them."
        ));
        assert!(!store.unread("sam").unwrap().contains(&fixtures::WATER));
        assert_eq!(store.history(4).unwrap().len(), 2);
        // Undo and toasts belong to the session that made the edit.
        assert!(store.take_flash("sam").is_none());
        assert!(matches!(store.undo("sam"), Err(Error::BadRequest(_))));
    }

    #[test]
    fn rejects_bad_rows() {
        let store = fixtures::store().unwrap();
        // The schema refuses a received message without a state, and a
        // folder that doesn't exist.
        let inner = store.lock();
        assert!(
            inner
                .conn
                .execute("UPDATE messages SET state = NULL WHERE id = 4", [])
                .is_err()
        );
        assert!(
            inner
                .conn
                .execute("UPDATE messages SET folder = 'Nope' WHERE id = 4", [])
                .is_err()
        );
        inner
            .conn
            .execute("UPDATE messages SET cc = 'not json' WHERE id = 4", [])
            .unwrap();
        drop(inner);
        assert!(matches!(store.message(4), Err(Error::Db(_))));
    }

    fn exec(store: &Store, sql: &str) {
        store.lock().conn.execute_batch(sql).unwrap();
    }

    #[test]
    fn failed_writes_roll_back_the_whole_edit() {
        let store = fixtures::store().unwrap();
        exec(
            &store,
            "CREATE TRIGGER no_history BEFORE INSERT ON history BEGIN SELECT RAISE(ABORT, 'no'); END;
             CREATE TRIGGER no_assignees BEFORE INSERT ON assignees BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        // The state update goes in before the history row that fails.
        assert!(matches!(
            store.edit("sam", 4, Change::State(State::Do)),
            Err(Error::Db(_))
        ));
        assert!(matches!(
            store.edit("sam", 4, Change::ToggleAssignee("alex".into())),
            Err(Error::Db(_))
        ));
        assert_eq!(values(&store, 4).state, State::Inbox);
        assert!(values(&store, 4).assignees.is_empty());
        assert!(store.history(4).unwrap().is_empty());
        assert!(store.take_flash("sam").is_none());

        // A failed undo stays undoable.
        exec(&store, "DROP TRIGGER no_history;");
        store.edit("sam", 4, Change::State(State::Do)).unwrap();
        exec(
            &store,
            "CREATE TRIGGER no_history BEFORE INSERT ON history BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        assert!(matches!(store.undo("sam"), Err(Error::Db(_))));
        assert_eq!(values(&store, 4).state, State::Do);
        exec(&store, "DROP TRIGGER no_history;");
        store.undo("sam").unwrap();
        assert_eq!(values(&store, 4).state, State::Inbox);
    }

    #[test]
    fn database_errors_surface() {
        let store = fixtures::store().unwrap();
        let msg = store.message(4).unwrap().unwrap();
        exec(&store, "PRAGMA query_only = ON;");
        let db_err = |r: Result<(), Error>| assert!(matches!(r, Err(Error::Db(_))), "{r:?}");
        db_err(store.edit("sam", 4, Change::State(State::Do)));
        db_err(store.mark_read("sam", 4));
        db_err(store.add_comment("sam", 1, "hi"));
        db_err(store.import(|tx| tx.folder("Travel")));
        db_err(store.import(|tx| tx.message(&msg)));
        exec(&store, "PRAGMA query_only = OFF;");

        // Each table the store reads, gone in turn.
        exec(&store, "ALTER TABLE folders RENAME TO gone_folders;");
        db_err(store.edit("sam", 4, Change::Folder(Some("House".into()))));
        assert!(store.folders().is_err());
        exec(&store, "ALTER TABLE comments RENAME TO gone_comments;");
        assert!(store.timeline(1).is_err());
        exec(&store, "ALTER TABLE reads RENAME TO gone_reads;");
        assert!(store.unread("sam").is_err());
        exec(&store, "ALTER TABLE history RENAME TO gone_history;");
        assert!(store.history(4).is_err());
        exec(&store, "ALTER TABLE threads RENAME TO gone_threads;");
        db_err(store.add_comment("sam", 1, "hi"));
        exec(&store, "ALTER TABLE users RENAME TO gone_users;");
        assert!(store.users().is_err());
        assert!(store.is_empty().is_err());
    }
}
