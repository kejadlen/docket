//! Docket's data in SQLite: a cache of the mail it tracks (from fixtures in
//! dev until JMAP import exists), and the state, assignees, reads, comments,
//! and history only Docket keeps. Every read is a query returning owned
//! values. Undo and toasts belong to the browsing session and stay in
//! memory.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use camino::Utf8Path;
use jiff::civil::DateTime;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{
    Connection, OptionalExtension as _, Params, Row, ToSql, Transaction, params, params_from_iter,
};

use crate::Error;
use crate::model::{
    Account, Comment, Event, Kind, Message, MessageId, State, Thread, ThreadId, User, Values,
};

/// Applied in order; `PRAGMA user_version` counts how many have run.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_jmap_import.sql"),
    include_str!("../migrations/0003_pending_files.sql"),
    include_str!("../migrations/0004_pending_deletes.sql"),
    include_str!("../migrations/0005_pending_archives.sql"),
    include_str!("../migrations/0006_jj_style_ids.sql"),
    include_str!("../migrations/0007_pending_moves.sql"),
    include_str!("../migrations/0008_adoption.sql"),
    include_str!("../migrations/0009_html.sql"),
];

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
    Event(Event),
}

/// Which messages a list asks for.
#[derive(Debug, Clone, Copy)]
pub enum Filter<'a> {
    /// Received messages in a state.
    State(State),
    /// Received messages in a state that are the user's: assigned to them,
    /// or unassigned.
    ForMe { user: &'a str, state: State },
    /// Messages whose subject, sender, or body contains the text. Case is
    /// ignored for ASCII letters only, as SQLite's LIKE does.
    Search(&'a str),
}

impl Filter<'_> {
    /// The WHERE clause over [`MESSAGE_JOINS`] and its arguments, shared
    /// by loading and counting so the two can't disagree.
    fn sql(self) -> (&'static str, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value::Text;
        match self {
            Filter::State(state) => (
                "m.kind = 'received' AND m.state = ?1",
                vec![Text(state.slug().to_owned())],
            ),
            Filter::ForMe { user, state } => (
                "m.kind = 'received' AND m.state = ?1 AND (
                    EXISTS (SELECT 1 FROM assignees a WHERE a.message = m.id AND a.user = ?2)
                    OR NOT EXISTS (SELECT 1 FROM assignees a WHERE a.message = m.id)
                )",
                vec![Text(state.slug().to_owned()), Text(user.to_owned())],
            ),
            Filter::Search(text) => (
                r"t.subject LIKE ?1 ESCAPE '\' OR m.from_name LIKE ?1 ESCAPE '\'
                    OR u.slug LIKE ?1 ESCAPE '\' OR m.body LIKE ?1 ESCAPE '\'",
                vec![Text(format!("%{}%", escape_like(text)))],
            ),
        }
    }
}

/// What a [`Filter`]'s clause can see besides the message: its thread
/// (`t`) and the user who sent it (`u`).
const MESSAGE_JOINS: &str =
    "JOIN threads t ON t.id = m.thread LEFT JOIN users u ON u.login = m.sent_by";

/// Where "now" comes from: the fixed clock fixtures are written against,
/// or the system clock for a live server.
#[derive(Debug, Clone, Copy)]
pub enum Clock {
    Fixed(DateTime),
    System,
}

impl Clock {
    fn now(self) -> DateTime {
        match self {
            Clock::Fixed(at) => at,
            Clock::System => jiff::Zoned::now().datetime(),
        }
    }
}

/// One connection behind a mutex: two people's clicks never contend long
/// enough to need a pool, and no lock is held across an await.
#[derive(Debug, Clone)]
pub struct Store {
    /// The clock events and comments are stamped with.
    clock: Clock,
    inner: Arc<Mutex<Inner>>,
}

#[derive(Debug)]
struct Inner {
    conn: Connection,
    undo: BTreeMap<String, (MessageId, Values, bool)>,
    flash: BTreeMap<String, Flash>,
}

impl Store {
    /// Opens (creating if needed) the database file and brings its schema
    /// up to date.
    pub fn open(path: &Utf8Path, clock: Clock) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn, clock)
    }

    /// A database that lasts as long as the store, for tests.
    pub fn open_in_memory(clock: Clock) -> Result<Self, Error> {
        Self::init(Connection::open_in_memory()?, clock)
    }

    fn init(mut conn: Connection, clock: Clock) -> Result<Self, Error> {
        conn.pragma_update(None, "foreign_keys", true)?;
        migrate(&mut conn)?;
        Ok(Self {
            clock,
            inner: Arc::new(Mutex::new(Inner {
                conn,
                undo: BTreeMap::new(),
                flash: BTreeMap::new(),
            })),
        })
    }

    /// The current time per this store's clock.
    pub fn now(&self) -> DateTime {
        self.clock.now()
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
            now: self.now(),
        };
        f(&import)?;
        import.tx.commit()?;
        Ok(())
    }

    pub fn users(&self) -> Result<Vec<User>, Error> {
        let inner = self.lock();
        let mut stmt = inner
            .conn
            .prepare_cached("SELECT login, slug FROM users ORDER BY rowid")?;
        let users = stmt
            .query_map([], user_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(users)
    }

    pub fn user(&self, login: &str) -> Result<Option<User>, Error> {
        load_user(&self.lock().conn, login)
    }

    /// The user behind a request Tailscale identified, added on their first
    /// request. Their slug follows whatever the proxy sends.
    pub fn sign_in(&self, login: &str, slug: &str) -> Result<User, Error> {
        let user = User::new(login, slug);
        self.lock().conn.execute(
            "INSERT INTO users (login, slug) VALUES (?1, ?2)
             ON CONFLICT (login) DO UPDATE SET slug = excluded.slug WHERE slug != excluded.slug",
            params![user.login, user.slug],
        )?;
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

    /// Every account, in the order sessions recorded them.
    pub fn accounts(&self) -> Result<Vec<Account>, Error> {
        let inner = self.lock();
        let mut stmt = inner
            .conn
            .prepare_cached("SELECT slug, name, address, read_only FROM accounts ORDER BY rowid")?;
        let accounts = stmt
            .query_map([], account_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(accounts)
    }

    /// True when the account's thread is already imported: the poll
    /// path admits a sent email only into a thread it knows.
    /// Whether the account already tracks the message, by Message-ID:
    /// mail that leaves the Inbox stays the poll's business once known.
    pub fn has_message(&self, account: &str, message_id: &str) -> Result<bool, Error> {
        let inner = self.lock();
        let known = inner.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE account = ?1 AND message_id = ?2)",
            [account, message_id],
            |r| r.get(0),
        )?;
        Ok(known)
    }

    pub fn has_thread(&self, account: &str, jmap_thread_id: &str) -> Result<bool, Error> {
        let found = self.lock().conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads
                  WHERE account = ?1 AND jmap_thread_id = ?2)",
            params![account, jmap_thread_id],
            |r| r.get(0),
        )?;
        Ok(found)
    }

    pub fn thread(&self, id: &str) -> Result<Option<Thread>, Error> {
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

    pub fn thread_account(&self, thread: &str) -> Result<Option<Account>, Error> {
        load_thread_account(&self.lock().conn, thread)
    }

    pub fn message(&self, id: &str) -> Result<Option<Message>, Error> {
        load_message(&self.lock().conn, id)
    }

    /// The thread's messages, oldest first.
    /// The message's HTML part as the server sent it: unsanitized, so
    /// it goes out only through [`crate::html::sanitize`]. None for
    /// text-only mail.
    pub fn html(&self, id: &str) -> Result<Option<String>, Error> {
        let inner = self.lock();
        let html = inner
            .conn
            .query_row("SELECT html FROM messages WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(html.flatten())
    }

    pub fn thread_messages(&self, thread: &str) -> Result<Vec<Message>, Error> {
        load_messages(&self.lock().conn, "m.thread = ?1", [thread])
    }

    /// Messages matching the filter, oldest first.
    pub fn messages(&self, filter: Filter<'_>) -> Result<Vec<Message>, Error> {
        let (clause, args) = filter.sql();
        load_messages(&self.lock().conn, clause, params_from_iter(args))
    }

    /// How many messages match the filter, without loading them.
    pub fn count(&self, filter: Filter<'_>) -> Result<usize, Error> {
        let (clause, args) = filter.sql();
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(&format!(
            "SELECT COUNT(*) FROM messages m {MESSAGE_JOINS} WHERE {clause}"
        ))?;
        let n: i64 = stmt.query_row(params_from_iter(args), |r| r.get(0))?;
        // COUNT(*) is never negative.
        Ok(usize::try_from(n).unwrap_or_default())
    }

    /// The threads with these ids, in one query.
    pub fn threads(&self, ids: &BTreeSet<ThreadId>) -> Result<BTreeMap<ThreadId, Thread>, Error> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT id, account, subject FROM threads
             WHERE id IN (SELECT value FROM json_each(?1))",
        )?;
        let threads = stmt
            .query_map([json(ids)?], |r| {
                Ok(Thread {
                    id: r.get(0)?,
                    account: r.get(1)?,
                    subject: r.get(2)?,
                })
            })?
            .map(|t| t.map(|t| (t.id.clone(), t)))
            .collect::<Result<_, _>>()?;
        Ok(threads)
    }

    /// The thread's messages, comments, and history events in time
    /// order. At the same moment a message sorts first, then comments,
    /// then events, which keep the order they happened in.
    pub fn timeline(&self, thread: &str) -> Result<Vec<Item>, Error> {
        let inner = self.lock();
        let messages = load_messages(&inner.conn, "m.thread = ?1", [thread])?;
        let mut stmt = inner.conn.prepare_cached(
            "SELECT id, thread, author, at, text FROM comments WHERE thread = ?1 ORDER BY at, rowid",
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
        let mut stmt = inner.conn.prepare_cached(
            "SELECT h.message, h.user, h.at, h.event
             FROM history h JOIN messages m ON m.id = h.message
             WHERE m.thread = ?1 ORDER BY h.rowid",
        )?;
        let events = stmt
            .query_map([thread], event_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut items: Vec<_> = messages
            .into_iter()
            .map(Item::Message)
            .chain(comments.into_iter().map(Item::Comment))
            .chain(events.into_iter().map(Item::Event))
            .collect();
        // Stable, so ties keep the chain order above.
        items.sort_by_key(|item| match item {
            Item::Message(m) => m.at,
            Item::Comment(c) => c.at,
            Item::Event(e) => e.at,
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

    pub fn mark_read(&self, user: &str, msg: &str) -> Result<(), Error> {
        insert_read(&self.lock().conn, user, msg)
    }

    /// Every change made to the message's values, oldest first.
    pub fn history(&self, message: &str) -> Result<Vec<Event>, Error> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT message, user, at, event FROM history WHERE message = ?1 ORDER BY rowid",
        )?;
        let events = stmt
            .query_map([message], event_from_row)?
            .collect::<Result<_, _>>()?;
        Ok(events)
    }

    pub fn edit(&self, user: &str, id: &str, change: Change) -> Result<(), Error> {
        let mut inner = self.lock();
        let tx = inner.conn.transaction()?;
        let msg = load_message(&tx, id)?.ok_or(Error::NotFound("message"))?;
        let read_only = load_thread_account(&tx, &msg.thread)?.is_some_and(|a| a.read_only);
        let Kind::Received { values, .. } = msg.kind else {
            return Err(Error::BadRequest("sent messages have no values"));
        };
        let prev = values;
        let mut next = prev.clone();
        // Whether this edit queued a server-side move, so an undo can
        // queue the move back.
        let mut queued_move = false;

        let text = match change {
            Change::State(state) => {
                next.state = state;
                // State is where the mail sits (DESIGN.md, State): on a
                // writable account the message moves to match.
                // Read-only accounts change state only.
                if !read_only {
                    queued_move = queue_move(&tx, id, state)?;
                }
                format!("Moved to {state}")
            }
            Change::Folder(folder) => {
                if read_only {
                    return Err(Error::Forbidden("this account is read-only"));
                }
                let text = match folder {
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
                        "Unfiled".to_owned()
                    }
                };
                queue_file(&tx, id, next.folder.as_deref())?;
                text
            }
            Change::ToggleAssignee(login) => {
                let name = load_user(&tx, &login)?.ok_or(Error::NotFound("user"))?.slug;
                if next.assignees.remove(&login) {
                    format!("Unassigned {name}")
                } else {
                    next.assignees.insert(login);
                    format!("Assigned {name}")
                }
            }
        };

        write_values(&tx, id, &next)?;
        insert_event(&tx, id, Some(user), self.now(), &text)?;
        tx.commit()?;
        inner
            .undo
            .insert(user.to_owned(), (id.to_owned(), prev, queued_move));
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
        let (id, prev, queued_move) = inner
            .undo
            .get(user)
            .cloned()
            .ok_or(Error::BadRequest("nothing to undo"))?;
        let tx = inner.conn.transaction()?;
        // An undone state change moves the mail back. If the poll
        // hadn't pushed the first move yet, the mail already sits where
        // the restored state says and the push finds nothing to do.
        if queued_move {
            let _queued = queue_move(&tx, &id, prev.state)?;
        }
        // An undone filing reverts server-side too: the queued intent
        // points back at the folder the undo restores.
        let msg = load_message(&tx, &id)?.ok_or(Error::NotFound("message"))?;
        if let Kind::Received { values, .. } = &msg.kind
            && values.folder != prev.folder
        {
            queue_file(&tx, &id, prev.folder.as_deref())?;
        }
        write_values(&tx, &id, &prev)?;
        insert_event(&tx, &id, Some(user), self.now(), "Undone")?;
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

    /// The account's queued filings, for the poll loop to push (task
    /// rn). The latest intent per message is the only row there is.
    pub fn pending_files(&self, account: &str) -> Result<Vec<PendingFile>, Error> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT p.message, m.jmap_id, p.folder
             FROM pending_files p JOIN messages m ON m.id = p.message
             WHERE m.account = ?1 ORDER BY p.message",
        )?;
        let pending = stmt
            .query_map([account], |r| {
                Ok(PendingFile {
                    message: r.get(0)?,
                    jmap_id: r.get(1)?,
                    folder: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(pending)
    }

    /// Drops queued filings once the server has taken them — or
    /// refused them, which no retry would fix.
    pub fn clear_pending_files(&self, messages: &[MessageId]) -> Result<(), Error> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        let tx = inner.conn.transaction()?;
        for id in messages {
            tx.execute("DELETE FROM pending_files WHERE message = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Deletes a message (task sm): queued for the poll loop to
    /// destroy server-side — which lands it in Trash — while the
    /// thread goes Done here with a history event. Any queued filing
    /// or move is superseded.
    pub fn delete(&self, user: &str, id: &str) -> Result<(), Error> {
        let mut inner = self.lock();
        let tx = inner.conn.transaction()?;
        let msg = load_message(&tx, id)?.ok_or(Error::NotFound("message"))?;
        let read_only = load_thread_account(&tx, &msg.thread)?.is_some_and(|a| a.read_only);
        if read_only {
            return Err(Error::Forbidden("this account is read-only"));
        }
        let prev = msg
            .values()
            .ok_or(Error::BadRequest("sent messages have no values"))?
            .clone();
        let mut next = prev;
        next.state = State::Done;
        tx.execute(
            "INSERT INTO pending_deletes (message)
             SELECT ?1 FROM messages WHERE id = ?1 AND jmap_id IS NOT NULL
             ON CONFLICT (message) DO NOTHING",
            [id],
        )?;
        tx.execute("DELETE FROM pending_files WHERE message = ?1", [id])?;
        tx.execute("DELETE FROM pending_moves WHERE message = ?1", [id])?;
        write_values(&tx, id, &next)?;
        insert_event(&tx, id, Some(user), self.now(), "Deleted")?;
        tx.commit()?;
        inner.flash.insert(
            user.to_owned(),
            Flash {
                text: "Deleted".into(),
                // The server-side trash can't be undone from here.
                undoable: false,
            },
        );
        Ok(())
    }

    /// The account's queued deletions, for the poll loop to push (task
    /// sm).
    pub fn pending_deletes(&self, account: &str) -> Result<Vec<PendingDelete>, Error> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT p.message, m.jmap_id
             FROM pending_deletes p JOIN messages m ON m.id = p.message
             WHERE m.account = ?1 ORDER BY p.message",
        )?;
        let pending = stmt
            .query_map([account], |r| {
                Ok(PendingDelete {
                    message: r.get(0)?,
                    jmap_id: r.get(1)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(pending)
    }

    /// Drops queued deletions once the server has taken them — or
    /// refused them, which no retry would fix.
    pub fn clear_pending_deletes(&self, messages: &[MessageId]) -> Result<(), Error> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        let tx = inner.conn.transaction()?;
        for id in messages {
            tx.execute("DELETE FROM pending_deletes WHERE message = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The account's queued state moves, for the poll loop to push. The
    /// latest state per message is the only row there is.
    pub fn pending_moves(&self, account: &str) -> Result<Vec<PendingMove>, Error> {
        let inner = self.lock();
        let mut stmt = inner.conn.prepare_cached(
            "SELECT p.message, m.jmap_id, p.state
             FROM pending_moves p JOIN messages m ON m.id = p.message
             WHERE m.account = ?1 ORDER BY p.message",
        )?;
        let pending = stmt
            .query_map([account], |r| {
                Ok(PendingMove {
                    message: r.get(0)?,
                    jmap_id: r.get(1)?,
                    state: r.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(pending)
    }

    /// Drops queued moves once the server has taken them — or refused
    /// them, or never needed them.
    pub fn clear_pending_moves(&self, messages: &[MessageId]) -> Result<(), Error> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut inner = self.lock();
        let tx = inner.conn.transaction()?;
        for id in messages {
            tx.execute("DELETE FROM pending_moves WHERE message = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn take_flash(&self, user: &str) -> Option<Flash> {
        self.lock().flash.remove(user)
    }

    pub fn add_comment(&self, user: &str, thread: &str, text: &str) -> Result<(), Error> {
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
        insert_comment(&inner.conn, None, thread, user, self.now(), text)?;
        Ok(())
    }

    /// A panic mid-request can't leave SQLite half-written (transactions
    /// roll back on drop), so a poisoned lock is safe to keep using.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Mail a JMAP session delivered, shaped for [`Import::incoming`].
#[derive(Debug, Clone)]
pub struct Incoming {
    /// The JMAP Email id, cached.
    pub jmap_id: String,
    /// The RFC 5322 Message-ID, without angle brackets.
    pub message_id: String,
    /// The JMAP threadId; threads upsert by it.
    pub jmap_thread_id: String,
    /// Names the thread on first import.
    pub subject: String,
    pub at: DateTime,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub body: String,
    /// The first text/html part, raw; sanitized when served.
    pub html: Option<String>,
    pub kind: IncomingKind,
}

#[derive(Debug, Clone)]
pub enum IncomingKind {
    Received {
        from: String,
        addr: String,
        /// Seeded from any `Docket/` label the message already carries,
        /// else Inbox.
        state: State,
        folder: Option<String>,
    },
    /// `by` is a login when the From address names one of us; None means
    /// the account's shared identity.
    Sent { by: Option<String>, to: Vec<String> },
}

/// One queued filing, waiting for the poll loop to push it (task rn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFile {
    pub message: MessageId,
    /// The JMAP Email id to move — cached on the message, read fresh at
    /// drain time so a reimport's new id is the one that goes out.
    pub jmap_id: String,
    /// The folder to file into; None unfiles.
    pub folder: Option<String>,
}

/// One queued deletion, waiting for the poll loop to push it (task
/// sm).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDelete {
    pub message: MessageId,
    /// The JMAP Email id to destroy — cached on the message, read fresh
    /// at drain time so a reimport's new id is the one that goes out.
    pub jmap_id: String,
}

/// One queued state move, waiting for the poll loop to push it into
/// the mailboxes that say `state` (DESIGN.md, State).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMove {
    pub message: MessageId,
    /// The JMAP Email id to move — cached on the message, read fresh at
    /// drain time so a reimport's new id is the one that goes out.
    pub jmap_id: String,
    pub state: State,
}

/// Writes inside one transaction, from [`Store::import`].
pub struct Import<'a> {
    tx: Transaction<'a>,
    /// The clock adopted changes are stamped with.
    now: DateTime,
}

impl Import<'_> {
    pub fn user(&self, user: &User) -> Result<(), Error> {
        self.tx.execute(
            "INSERT INTO users (login, slug) VALUES (?1, ?2)",
            params![user.login, user.slug],
        )?;
        Ok(())
    }

    /// An account from a session. Upserted: the first import inserts, and
    /// every session refresh re-writes the name, address, and read-only
    /// flag as the server now reports them.
    pub fn account(&self, account: &Account) -> Result<(), Error> {
        self.tx.execute(
            "INSERT INTO accounts (slug, name, address, read_only) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (slug) DO UPDATE SET
                 name = excluded.name,
                 address = excluded.address,
                 read_only = excluded.read_only",
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
        self.tx.execute(
            "INSERT INTO folders (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            [name],
        )?;
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
            Kind::Sent { by, to } => (
                "sent",
                None,
                None,
                None,
                None,
                by.as_deref(),
                Some(json(to)?),
            ),
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
            write_values(&self.tx, &m.id, values)?;
        }
        Ok(())
    }

    /// Gives an imported message an HTML part, as fixtures do; JMAP
    /// mail brings its own through [`Import::incoming`].
    pub fn html(&self, id: &str, raw: &str) -> Result<(), Error> {
        let updated = self
            .tx
            .execute("UPDATE messages SET html = ?2 WHERE id = ?1", [id, raw])?;
        if updated == 0 {
            return Err(Error::NotFound("message"));
        }
        Ok(())
    }

    pub fn comment(&self, c: &Comment) -> Result<(), Error> {
        insert_comment(&self.tx, Some(&c.id), &c.thread, &c.author, c.at, &c.text)
    }

    pub fn read(&self, user: &str, msg: &str) -> Result<(), Error> {
        insert_read(&self.tx, user, msg)
    }

    /// Mail a JMAP session delivered, keyed by account + Message-ID so
    /// re-importing is idempotent. The thread upserts by account + JMAP
    /// threadId, named by the first message that arrives; Docket-owned
    /// values (state, assignees, reads) are written only on first
    /// import, while the server-owned cache (folder, timestamps, body)
    /// refreshes.
    ///
    /// The one exception is state changed in another client (task qwt):
    /// when the mailboxes say a new state since the last sight, Docket
    /// adopts it with a history event rather than reverting it. A queued
    /// move or deletion is Docket's newer word and holds; read-only
    /// accounts keep state in the database alone, so nothing there
    /// adopts.
    pub fn incoming(&self, account: &str, mail: &Incoming) -> Result<(), Error> {
        let prior = self.prior(account, &mail.message_id)?;
        self.tx.execute(
            "INSERT INTO threads (id, account, subject, jmap_thread_id)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account, jmap_thread_id) DO NOTHING",
            params![
                fresh_id(&self.tx)?,
                account,
                mail.subject,
                mail.jmap_thread_id
            ],
        )?;
        let (kind, from_name, from_addr, state, folder, sent_by, sent_to) = match &mail.kind {
            IncomingKind::Received {
                from,
                addr,
                state,
                folder,
            } => (
                "received",
                Some(from),
                Some(addr),
                Some(state),
                folder.as_deref(),
                None,
                None,
            ),
            IncomingKind::Sent { by, to } => (
                "sent",
                None,
                None,
                None,
                None,
                by.as_deref(),
                Some(json(to)?),
            ),
        };
        let cc = json(&mail.cc)?;
        let bcc = json(&mail.bcc)?;
        let _upserted = self.tx.execute(
            "INSERT INTO messages (id, account, message_id, jmap_id, thread, at, cc, bcc, body,
                kind, from_name, from_addr, state, folder, sent_by, sent_to, server_state, html)
             SELECT ?1, ?2, ?3, ?4, t.id, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?13, ?17
             FROM threads t WHERE t.account = ?2 AND t.jmap_thread_id = ?5
             ON CONFLICT (account, message_id) DO UPDATE SET
                 server_state = excluded.server_state,
                 html = excluded.html,
                 jmap_id = excluded.jmap_id,
                 thread = excluded.thread,
                 at = excluded.at,
                 cc = excluded.cc,
                 bcc = excluded.bcc,
                 body = excluded.body,
                 from_name = excluded.from_name,
                 from_addr = excluded.from_addr,
                 folder = excluded.folder,
                 sent_by = excluded.sent_by,
                 sent_to = excluded.sent_to",
            params![
                fresh_id(&self.tx)?,
                account,
                mail.message_id,
                mail.jmap_id,
                mail.jmap_thread_id,
                mail.at,
                cc,
                bcc,
                mail.body,
                kind,
                from_name,
                from_addr,
                state,
                folder,
                sent_by,
                sent_to,
                mail.html,
            ],
        )?;
        if let (Some(prior), Some(seen)) = (prior, state)
            && prior.adopts(*seen)
        {
            self.tx.execute(
                "UPDATE messages SET state = ?2 WHERE id = ?1",
                params![prior.id, seen],
            )?;
            insert_event(
                &self.tx,
                &prior.id,
                None,
                self.now,
                &format!("Moved to {seen} via another client"),
            )?;
        }
        Ok(())
    }

    /// What a received message held before this sight, for adoption.
    fn prior(&self, account: &str, message_id: &str) -> Result<Option<Prior>, Error> {
        let prior = self
            .tx
            .query_row(
                "SELECT m.id, m.state, m.server_state,
                     EXISTS (SELECT 1 FROM pending_moves p WHERE p.message = m.id)
                     OR EXISTS (SELECT 1 FROM pending_deletes d WHERE d.message = m.id),
                     a.read_only
                 FROM messages m JOIN accounts a ON a.slug = m.account
                 WHERE m.account = ?1 AND m.message_id = ?2 AND m.kind = 'received'",
                [account, message_id],
                |r| {
                    Ok(Prior {
                        id: r.get(0)?,
                        state: r.get(1)?,
                        server_state: r.get(2)?,
                        queued: r.get(3)?,
                        read_only: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(prior)
    }
}

/// A received message as it stood before a sight of it.
struct Prior {
    id: MessageId,
    state: State,
    /// What the mailboxes said last time; None before any sight.
    server_state: Option<State>,
    /// A move or deletion Docket has yet to push.
    queued: bool,
    read_only: bool,
}

impl Prior {
    /// Whether the mailboxes' `seen` state is another client's change
    /// to adopt: they moved since the last sight, to somewhere Docket
    /// doesn't already say, with nothing of Docket's own in flight.
    fn adopts(&self, seen: State) -> bool {
        self.server_state.is_some_and(|last| last != seen)
            && self.state != seen
            && !self.queued
            && !self.read_only
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
        // A migration may rebuild a table (0002 does), which drops and
        // renames; FK enforcement would trap the drop, and the copy
        // inside is exact.
        conn.pragma_update(None, "foreign_keys", false)?;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", n)?;
        tx.commit()?;
        conn.pragma_update(None, "foreign_keys", true)?;
    }
    Ok(())
}

fn event_from_row(r: &Row<'_>) -> rusqlite::Result<Event> {
    Ok(Event {
        message: r.get(0)?,
        user: r.get(1)?,
        at: r.get(2)?,
        text: r.get(3)?,
    })
}

fn user_from_row(r: &Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        login: r.get(0)?,
        slug: r.get(1)?,
    })
}

fn load_user(conn: &Connection, login: &str) -> Result<Option<User>, Error> {
    let user = conn
        .query_row(
            "SELECT login, slug FROM users WHERE login = ?1",
            [login],
            user_from_row,
        )
        .optional()?;
    Ok(user)
}

fn load_thread_account(conn: &Connection, thread: &str) -> Result<Option<Account>, Error> {
    let account = conn
        .query_row(
            "SELECT a.slug, a.name, a.address, a.read_only
             FROM accounts a JOIN threads t ON t.account = a.slug WHERE t.id = ?1",
            [thread],
            account_from_row,
        )
        .optional()?;
    Ok(account)
}

fn account_from_row(r: &Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        slug: r.get(0)?,
        name: r.get(1)?,
        address: r.get(2)?,
        read_only: r.get(3)?,
    })
}

fn folder_exists(conn: &Connection, name: &str) -> Result<bool, Error> {
    let exists = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM folders WHERE name = ?1)",
        [name],
        |r| r.get(0),
    )?;
    Ok(exists)
}

fn load_message(conn: &Connection, id: &str) -> Result<Option<Message>, Error> {
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
                FROM (SELECT user FROM assignees WHERE message = m.id ORDER BY user)),
            m.html IS NOT NULL
         FROM messages m {MESSAGE_JOINS}
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
        has_html: r.get(15)?,
        kind,
    })
}

fn write_values(conn: &Connection, id: &str, values: &Values) -> Result<(), Error> {
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

/// Records a filing for the poll loop to push (task rn). Fixture mail
/// has no server side to move, so rows without a cached JMAP id stay
/// out.
fn queue_file(conn: &Connection, id: &str, folder: Option<&str>) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO pending_files (message, folder)
         SELECT ?1, ?2 FROM messages WHERE id = ?1 AND jmap_id IS NOT NULL
         ON CONFLICT (message) DO UPDATE SET folder = excluded.folder",
        params![id, folder],
    )?;
    Ok(())
}

/// Queues the move into the mailboxes that say `state`, on a writable
/// account. Fixture mail has no server side to move, so rows without a
/// cached JMAP id stay out. True when a move was queued.
fn queue_move(conn: &Connection, id: &str, state: State) -> Result<bool, Error> {
    let queued = conn.execute(
        "INSERT INTO pending_moves (message, state)
         SELECT ?1, ?2 FROM messages WHERE id = ?1 AND jmap_id IS NOT NULL
         ON CONFLICT (message) DO UPDATE SET state = excluded.state",
        params![id, state],
    )?;
    Ok(queued > 0)
}

/// `user` is None for a change adopted from another client.
fn insert_event(
    conn: &Connection,
    message: &str,
    user: Option<&str>,
    at: DateTime,
    text: &str,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO history (id, message, user, at, event) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![fresh_id(conn)?, message, user, at, text],
    )?;
    Ok(())
}

fn insert_comment(
    conn: &Connection,
    id: Option<&str>,
    thread: &str,
    author: &str,
    at: DateTime,
    text: &str,
) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO comments (id, thread, author, at, text) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id.unwrap_or(&fresh_id(conn)?), thread, author, at, text],
    )?;
    Ok(())
}

fn insert_read(conn: &Connection, user: &str, msg: &str) -> Result<(), Error> {
    conn.execute(
        "INSERT OR IGNORE INTO reads (user, message) VALUES (?1, ?2)",
        params![user, msg],
    )?;
    Ok(())
}

/// A fresh row id: 64 random bits as jj-style reverse hex — jj's
/// change-id alphabet, where nibble `0` renders as `z` — giving 16
/// characters that carry no order and no meaning.
fn fresh_id(conn: &Connection) -> Result<String, Error> {
    let bytes: Vec<u8> = conn.query_row("SELECT randomblob(8)", [], |r| r.get(0))?;
    Ok(crate::model::reverse_hex_bytes(&bytes))
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
    use crate::fixtures::{self, ALEX, SAM};

    fn values(store: &Store, id: &str) -> Values {
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
        store
            .edit(SAM, &fixtures::id(4), Change::State(State::Do))
            .unwrap();
        assert_eq!(values(&store, &fixtures::id(4)).state, State::Do);
        assert_eq!(
            store.take_flash(SAM),
            Some(Flash {
                text: "Moved to Do".into(),
                undoable: true
            })
        );
        assert_eq!(store.take_flash(SAM), None);

        store.undo(SAM).unwrap();
        assert_eq!(values(&store, &fixtures::id(4)).state, State::Inbox);
        assert_eq!(store.take_flash(SAM).unwrap().text, "Undone");
        assert!(matches!(store.undo(SAM), Err(Error::BadRequest(_))));

        let history: Vec<_> = store
            .history(&fixtures::id(4))
            .unwrap()
            .into_iter()
            .map(|e| (e.user, e.at, e.text))
            .collect();
        assert_eq!(
            history,
            [
                (Some(SAM.into()), store.now(), "Moved to Do".into()),
                (Some(SAM.into()), store.now(), "Undone".into()),
            ]
        );
    }

    #[test]
    fn filing_queues_intent_for_the_poll_loop() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })?;
                tx.folder("House")?;
                tx.incoming("household", &incoming(State::Inbox, None, "$1,840"))
            })
            .unwrap();
        let id = store
            .messages(Filter::Search("1,840"))
            .unwrap()
            .first()
            .unwrap()
            .id
            .clone();
        store.sign_in(SAM, "Sam").unwrap();

        // Filing queues the intent, and the latest one wins.
        store
            .edit(SAM, &id, Change::Folder(Some("House".into())))
            .unwrap();
        store.edit(SAM, &id, Change::Folder(None)).unwrap();
        assert_eq!(
            store.pending_files("household").unwrap(),
            [PendingFile {
                message: id.clone(),
                jmap_id: "E9".into(),
                folder: None,
            }]
        );

        // Another account's drain never sees them.
        assert!(store.pending_files("eli").unwrap().is_empty());

        // What the poll loop pushed, it clears.
        store.clear_pending_files(&[id.clone()]).unwrap();
        assert!(store.pending_files("household").unwrap().is_empty());
        store.clear_pending_files(&[]).unwrap();

        // Undoing a filing re-points the queue at the restored folder.
        store
            .edit(SAM, &id, Change::Folder(Some("House".into())))
            .unwrap();
        assert_eq!(
            store.pending_files("household").unwrap()[0].folder,
            Some("House".into())
        );
        store.undo(SAM).unwrap();
        assert_eq!(store.pending_files("household").unwrap()[0].folder, None);

        // A filing that can't queue fails the whole edit, and the
        // folder it would have set rolls back.
        exec(
            &store,
            "CREATE TRIGGER no_pending BEFORE INSERT ON pending_files BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        assert!(matches!(
            store.edit(SAM, &id, Change::Folder(Some("House".into()))),
            Err(Error::Db(_))
        ));
        exec(&store, "DROP TRIGGER no_pending;");
        assert_eq!(values(&store, &id).folder, None);
    }

    #[test]
    fn a_gone_pending_table_fails_the_drain() {
        // A fresh store, so the query compiles against the missing
        // table rather than replaying a cached statement.
        let store = fixtures::store().unwrap();
        exec(&store, "DROP TABLE pending_files;");
        assert!(store.pending_files("household").is_err());
    }

    #[test]
    fn fixture_mail_never_queues_a_filing() {
        let store = fixtures::store().unwrap();
        store
            .edit(
                SAM,
                &fixtures::id(4),
                Change::Folder(Some("Finance".into())),
            )
            .unwrap();
        // No server side to move, so no row: fixture mail has no cached
        // JMAP id.
        assert!(store.pending_files("household").unwrap().is_empty());
    }

    #[test]
    fn folders() {
        let store = fixtures::store().unwrap();
        store
            .edit(
                SAM,
                &fixtures::id(4),
                Change::Folder(Some("Finance".into())),
            )
            .unwrap();
        assert_eq!(
            values(&store, &fixtures::id(4)).folder.as_deref(),
            Some("Finance")
        );
        store
            .edit(SAM, &fixtures::id(4), Change::Folder(None))
            .unwrap();
        assert_eq!(values(&store, &fixtures::id(4)).folder, None);
        assert_eq!(store.take_flash(SAM).unwrap().text, "Unfiled");
        assert!(matches!(
            store.edit(SAM, &fixtures::id(4), Change::Folder(Some("Nope".into()))),
            Err(Error::NotFound("folder"))
        ));
        assert_eq!(
            store.folders().unwrap(),
            ["Medical", "School", "House", "Finance"]
        );
    }

    #[test]
    fn done_queues_the_exit_from_the_shared_inbox() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })?;
                tx.incoming("household", &incoming(State::Inbox, None, "$1,840"))
            })
            .unwrap();
        let id = store
            .messages(Filter::Search("1,840"))
            .unwrap()
            .first()
            .unwrap()
            .id
            .clone();
        store.sign_in(SAM, "Sam").unwrap();

        // Done on a writable account queues the move.
        store.edit(SAM, &id, Change::State(State::Done)).unwrap();
        assert_eq!(values(&store, &id).state, State::Done);
        assert_eq!(
            store.pending_moves("household").unwrap(),
            [PendingMove {
                message: id.clone(),
                jmap_id: "E9".into(),
                state: State::Done,
            }]
        );
        assert_eq!(
            store.take_flash(SAM).unwrap(),
            Flash {
                text: "Moved to Done".into(),
                undoable: true,
            }
        );
        let history: Vec<_> = store
            .history(&id)
            .unwrap()
            .into_iter()
            .map(|e| (e.user, e.text))
            .collect();
        assert_eq!(history, [(Some(SAM.into()), "Moved to Done".into())]);

        // Undo reverts the state and queues the move back.
        store.undo(SAM).unwrap();
        assert_eq!(values(&store, &id).state, State::Inbox);
        assert_eq!(
            store.pending_moves("household").unwrap()[0].state,
            State::Inbox
        );

        // The other lanes leave the Inbox too; the latest state is the
        // one that goes out.
        store.edit(SAM, &id, Change::State(State::Wait)).unwrap();
        assert_eq!(store.take_flash(SAM).unwrap().text, "Moved to Wait");
        store.edit(SAM, &id, Change::State(State::Inbox)).unwrap();
        assert_eq!(store.take_flash(SAM).unwrap().text, "Moved to Inbox");
        assert_eq!(
            store.pending_moves("household").unwrap()[0].state,
            State::Inbox
        );

        // Another account's drain never sees them; what the poll
        // pushed, it clears.
        store.edit(SAM, &id, Change::State(State::Done)).unwrap();
        assert!(store.pending_moves("eli").unwrap().is_empty());
        store.clear_pending_moves(&[id.clone()]).unwrap();
        assert!(store.pending_moves("household").unwrap().is_empty());
        store.clear_pending_moves(&[]).unwrap();

        // Deleting a Done message supersedes its queued move.
        store.edit(SAM, &id, Change::State(State::Done)).unwrap();
        store.delete(SAM, &id).unwrap();
        assert!(store.pending_moves("household").unwrap().is_empty());
        assert_eq!(store.pending_deletes("household").unwrap().len(), 1);

        // A state change that can't queue fails the whole edit, leaving
        // the state alone.
        store.edit(SAM, &id, Change::State(State::Inbox)).unwrap();
        exec(
            &store,
            "CREATE TRIGGER no_pending_move BEFORE INSERT ON pending_moves BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        store.clear_pending_moves(&[id.clone()]).unwrap();
        assert!(matches!(
            store.edit(SAM, &id, Change::State(State::Done)),
            Err(Error::Db(_))
        ));
        assert_eq!(values(&store, &id).state, State::Inbox);

        // So does an undo that can't queue the move back.
        exec(&store, "DROP TRIGGER no_pending_move;");
        store.edit(SAM, &id, Change::State(State::Do)).unwrap();
        store.clear_pending_moves(&[id.clone()]).unwrap();
        exec(
            &store,
            "CREATE TRIGGER no_pending_move BEFORE INSERT ON pending_moves BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        assert!(matches!(store.undo(SAM), Err(Error::Db(_))));
        exec(&store, "DROP TRIGGER no_pending_move;");
        assert_eq!(values(&store, &id).state, State::Do);
    }

    #[test]
    fn state_without_a_server_side_stays_docket_local() {
        // Read-only accounts and fixture mail change state without
        // queueing anything.
        let store = fixtures::store().unwrap();
        let eli = fixtures::id(fixtures::ELI_PRACTICE);
        store.edit(SAM, &eli, Change::State(State::Done)).unwrap();
        assert!(store.pending_moves("eli").unwrap().is_empty());
        assert_eq!(store.take_flash(SAM).unwrap().text, "Moved to Done");
        store.edit(SAM, &eli, Change::State(State::Do)).unwrap();
        assert_eq!(store.take_flash(SAM).unwrap().text, "Moved to Do");
        store
            .edit(SAM, &fixtures::id(4), Change::State(State::Done))
            .unwrap();
        assert!(store.pending_moves("household").unwrap().is_empty());
    }

    #[test]
    fn a_gone_pending_moves_table_fails_the_drain() {
        // A fresh store, so the query compiles against the missing
        // table rather than replaying a cached statement.
        let store = fixtures::store().unwrap();
        exec(&store, "DROP TABLE pending_moves;");
        assert!(store.pending_moves("household").is_err());
    }

    #[test]
    fn deleting_marks_done_and_queues_the_destroy() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })?;
                tx.folder("House")?;
                tx.incoming("household", &incoming(State::Inbox, None, "$1,840"))
            })
            .unwrap();
        let id = store
            .messages(Filter::Search("1,840"))
            .unwrap()
            .first()
            .unwrap()
            .id
            .clone();
        store.sign_in(SAM, "Sam").unwrap();

        // A queued filing is superseded: the destroy replaces it.
        store
            .edit(SAM, &id, Change::Folder(Some("House".into())))
            .unwrap();
        store.delete(SAM, &id).unwrap();
        assert_eq!(values(&store, &id).state, State::Done);
        assert_eq!(
            store.pending_deletes("household").unwrap(),
            [PendingDelete {
                message: id.clone(),
                jmap_id: "E9".into(),
            }]
        );
        assert!(store.pending_files("household").unwrap().is_empty());
        assert_eq!(
            store.take_flash(SAM),
            Some(Flash {
                text: "Deleted".into(),
                undoable: false
            })
        );
        let history: Vec<_> = store
            .history(&id)
            .unwrap()
            .into_iter()
            .map(|e| (e.user, e.text))
            .collect();
        assert_eq!(
            history,
            [
                (Some(SAM.into()), "Filed to House".into()),
                (Some(SAM.into()), "Deleted".into())
            ]
        );

        // Another account's drain never sees them; what the poll
        // pushed, it clears.
        assert!(store.pending_deletes("eli").unwrap().is_empty());
        store.clear_pending_deletes(&[id.clone()]).unwrap();
        assert!(store.pending_deletes("household").unwrap().is_empty());
        store.clear_pending_deletes(&[]).unwrap();

        // A deletion that can't queue fails, leaving the state alone.
        exec(
            &store,
            "CREATE TRIGGER no_pending_delete BEFORE INSERT ON pending_deletes BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        store.edit(SAM, &id, Change::State(State::Inbox)).unwrap();
        assert!(matches!(store.delete(SAM, &id), Err(Error::Db(_))));
        exec(&store, "DROP TRIGGER no_pending_delete;");
        assert_eq!(values(&store, &id).state, State::Inbox);
    }

    #[test]
    fn a_gone_pending_deletes_table_fails_the_drain() {
        // A fresh store, so the query compiles against the missing
        // table rather than replaying a cached statement.
        let store = fixtures::store().unwrap();
        exec(&store, "DROP TABLE pending_deletes;");
        assert!(store.pending_deletes("household").is_err());
    }

    #[test]
    fn read_only_accounts_cannot_file() {
        let store = fixtures::store().unwrap();
        let eli = fixtures::id(fixtures::ELI_PRACTICE);
        assert!(matches!(
            store.edit(SAM, &eli, Change::Folder(None)),
            Err(Error::Forbidden(_))
        ));
        assert!(matches!(store.delete(SAM, &eli), Err(Error::Forbidden(_))));
        store.edit(SAM, &eli, Change::State(State::Done)).unwrap();
    }

    #[test]
    fn fixture_and_sent_mail_delete_differently() {
        let store = fixtures::store().unwrap();
        // Fixture mail has no server side to trash, so no row queues —
        // but the thread still goes Done.
        store.delete(SAM, &fixtures::id(4)).unwrap();
        assert_eq!(values(&store, &fixtures::id(4)).state, State::Done);
        assert!(store.pending_deletes("household").unwrap().is_empty());
        // Sent replies carry no values to transition.
        assert!(matches!(
            store.delete(SAM, &fixtures::id(2)),
            Err(Error::BadRequest(_))
        ));
    }

    #[test]
    fn assignees_toggle() {
        let store = fixtures::store().unwrap();
        store
            .edit(ALEX, &fixtures::id(4), Change::ToggleAssignee(ALEX.into()))
            .unwrap();
        assert!(values(&store, &fixtures::id(4)).assignees.contains(ALEX));
        assert_eq!(store.take_flash(ALEX).unwrap().text, "Assigned Alex");
        store
            .edit(ALEX, &fixtures::id(4), Change::ToggleAssignee(ALEX.into()))
            .unwrap();
        assert!(values(&store, &fixtures::id(4)).assignees.is_empty());
        assert_eq!(store.take_flash(ALEX).unwrap().text, "Unassigned Alex");
        assert!(matches!(
            store.edit(ALEX, &fixtures::id(4), Change::ToggleAssignee("eli".into())),
            Err(Error::NotFound("user"))
        ));
    }

    #[test]
    fn bad_edits() {
        let store = fixtures::store().unwrap();
        assert!(matches!(
            store.edit(SAM, &fixtures::id(999), Change::State(State::Do)),
            Err(Error::NotFound("message"))
        ));
        assert!(matches!(
            store.edit(SAM, &fixtures::id(2), Change::State(State::Do)),
            Err(Error::BadRequest(_))
        ));
    }

    #[test]
    fn comments() {
        let store = fixtures::store().unwrap();
        let before = store.timeline(&fixtures::id(1)).unwrap().len();
        store
            .add_comment(SAM, &fixtures::id(1), "  Called them.  ")
            .unwrap();
        let timeline = store.timeline(&fixtures::id(1)).unwrap();
        assert_eq!(timeline.len(), before + 1);
        assert!(matches!(timeline.last(), Some(Item::Comment(c)) if c.text == "Called them."));
        assert!(matches!(
            store.add_comment(SAM, &fixtures::id(1), "   "),
            Err(Error::BadRequest(_))
        ));
        assert!(matches!(
            store.add_comment(SAM, &fixtures::id(999), "hi"),
            Err(Error::NotFound("thread"))
        ));
    }

    #[test]
    fn timeline_interleaves_comments_and_events_by_time() {
        let store = fixtures::store().unwrap();
        // Edits stamp the fixed clock, after every fixture message; the
        // two events keep the order they happened in.
        store
            .edit(SAM, &fixtures::id(4), Change::State(State::Do))
            .unwrap();
        store
            .edit(SAM, &fixtures::id(3), Change::Folder(None))
            .unwrap();
        // Another thread's history stays out.
        store
            .edit(SAM, &fixtures::id(5), Change::State(State::Done))
            .unwrap();
        let order: Vec<_> = store
            .timeline(&fixtures::id(1))
            .unwrap()
            .into_iter()
            .map(|item| match item {
                Item::Message(m) => format!("m{}", m.id),
                Item::Comment(c) => format!("c{}", c.id),
                Item::Event(e) => format!("e{} {}", e.message, e.text),
            })
            .collect();
        assert_eq!(
            order,
            [
                format!("m{}", fixtures::id(1)),
                format!("c{}", fixtures::id(1)),
                format!("m{}", fixtures::id(2)),
                format!("m{}", fixtures::id(3)),
                format!("m{}", fixtures::id(4)),
                format!("e{} Moved to Do", fixtures::id(4)),
                format!("e{} Unfiled", fixtures::id(3)),
            ]
        );

        // A fresh store, so the history query prepares against the gap.
        let store = fixtures::store().unwrap();
        exec(&store, "ALTER TABLE history RENAME TO gone_history;");
        assert!(store.timeline(&fixtures::id(1)).is_err());
    }

    #[test]
    fn read_tracking() {
        let store = fixtures::store().unwrap();
        assert!(
            store
                .unread(ALEX)
                .unwrap()
                .contains(&fixtures::id(fixtures::WATER))
        );
        store
            .mark_read(ALEX, &fixtures::id(fixtures::WATER))
            .unwrap();
        store
            .mark_read(ALEX, &fixtures::id(fixtures::WATER))
            .unwrap();
        assert!(
            !store
                .unread(ALEX)
                .unwrap()
                .contains(&fixtures::id(fixtures::WATER))
        );
        assert!(
            store
                .unread(SAM)
                .unwrap()
                .contains(&fixtures::id(fixtures::WATER))
        );
        // Sent messages are never unread.
        assert!(!store.unread(SAM).unwrap().contains(&fixtures::id(2)));
    }

    #[test]
    fn messages_round_trip() {
        let store = fixtures::store().unwrap();
        let sent = store.message(&fixtures::id(2)).unwrap().unwrap();
        assert_eq!(sent.message_id, fixtures::message_id(2));
        assert_eq!(sent.cc, ["Alex"]);
        assert_eq!(sent.bcc, ["Pat Lee"]);
        assert!(
            matches!(&sent.kind, Kind::Sent { by, to } if by == &Some(SAM.to_owned())
                && to == &["Northwind Roofing".to_string()])
        );
        let got = store.message(&fixtures::id(5)).unwrap().unwrap();
        assert_eq!(got.at, jiff::civil::date(2026, 9, 30).at(15, 30, 0, 0));
        assert_eq!(
            got.values(),
            Some(&Values {
                state: State::Do,
                folder: Some("School".into()),
                assignees: BTreeSet::from([SAM.to_owned()]),
            })
        );
        assert!(store.message(&fixtures::id(999)).unwrap().is_none());
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
        let logins: Vec<_> = store
            .users()
            .unwrap()
            .into_iter()
            .map(|u| u.login)
            .collect();
        assert_eq!(logins, [ALEX, SAM]);
        assert_eq!(store.user(SAM).unwrap().unwrap().slug, "Sam");
        assert!(store.user("pat@example.com").unwrap().is_none());
        assert_eq!(
            store.thread(&fixtures::id(1)).unwrap().unwrap().subject,
            "Gutter repair estimate"
        );
        assert!(store.thread(&fixtures::id(999)).unwrap().is_none());
        assert!(
            store
                .thread_account(&fixtures::id(11))
                .unwrap()
                .unwrap()
                .read_only
        );
        assert!(store.thread_account(&fixtures::id(999)).unwrap().is_none());
    }

    #[test]
    fn a_session_refresh_rewrites_an_account() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        let mut account = Account {
            slug: "eli".into(),
            name: "Eli".into(),
            address: "eli@example.com".into(),
            read_only: true,
        };
        store.import(|tx| tx.account(&account)).unwrap();

        // A later session reports the account differently: same slug, new
        // facts. The import upserts rather than colliding.
        account.name = "Eli R.".into();
        account.address = "eli@chislan.family".into();
        account.read_only = false;
        store.import(|tx| tx.account(&account)).unwrap();
        store
            .import(|tx| {
                tx.thread(&Thread {
                    id: fixtures::id(1),
                    account: "eli".into(),
                    subject: "Practice".into(),
                })
            })
            .unwrap();

        assert_eq!(store.accounts().unwrap(), [account]);
        assert!(
            !store
                .thread_account(&fixtures::id(1))
                .unwrap()
                .unwrap()
                .read_only
        );
    }

    /// The shape of a JMAP delivery, for the incoming() tests.
    fn incoming(state: State, folder: Option<&str>, body: &str) -> Incoming {
        Incoming {
            jmap_id: "E9".into(),
            message_id: "m9@chislan.family".into(),
            jmap_thread_id: "T9".into(),
            subject: "Gutter repair estimate".into(),
            at: fixtures::now(),
            cc: Vec::new(),
            bcc: Vec::new(),
            body: body.to_owned(),
            html: None,
            kind: IncomingKind::Received {
                from: "Northwind Roofing".into(),
                addr: "office@northwind.co".into(),
                state,
                folder: folder.map(str::to_owned),
            },
        }
    }

    #[test]
    fn has_thread_reports_whether_a_threads_imported() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })?;
                tx.incoming("household", &incoming(State::Inbox, None, "Body"))
            })
            .unwrap();

        assert!(store.has_thread("household", "T9").unwrap());
        assert!(!store.has_thread("household", "T8").unwrap());
        // Threads are scoped to the account.
        assert!(!store.has_thread("eli", "T9").unwrap());
        assert!(store.has_message("household", "m9@chislan.family").unwrap());
        assert!(!store.has_message("eli", "m9@chislan.family").unwrap());

        exec(&store, "ALTER TABLE messages RENAME TO gone_messages;");
        assert!(store.has_message("household", "m9@chislan.family").is_err());
        exec(&store, "ALTER TABLE gone_messages RENAME TO messages;");
        exec(&store, "ALTER TABLE threads RENAME TO gone_threads;");
        assert!(store.has_thread("household", "T9").is_err());
    }

    #[test]
    fn incoming_mail_upserts_keeping_docket_owned_values() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })?;
                tx.folder("House")
            })
            .unwrap();

        store
            .import(|tx| {
                tx.incoming(
                    "household",
                    &incoming(State::Watch, Some("House"), "$1,840"),
                )
            })
            .unwrap();
        let id = store
            .messages(Filter::Search("1,840"))
            .unwrap()
            .first()
            .unwrap()
            .id
            .clone();
        // Triaged in Docket after the import: the state moves here.
        store.sign_in(ALEX, "Alex").unwrap();
        store.edit(ALEX, &id, Change::State(State::Do)).unwrap();

        // The next session reports a new body and no folder; the state we
        // set survives, the server's fields refresh.
        store
            .import(|tx| tx.incoming("household", &incoming(State::Inbox, None, "$2,120")))
            .unwrap();
        assert_eq!(store.messages(Filter::Search("2,120")).unwrap().len(), 1);
        let after = store.message(&id).unwrap().unwrap();
        assert_eq!(after.body, "$2,120");
        assert_eq!(after.values().unwrap().state, State::Do);
        assert_eq!(after.values().unwrap().folder, None);
    }

    #[test]
    fn incoming_adopts_a_state_changed_elsewhere() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        let sight =
            |state| store.import(|tx| tx.incoming("household", &incoming(state, None, "x")));
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })
            })
            .unwrap();
        sight(State::Inbox).unwrap();
        let id = store.messages(Filter::Search("x")).unwrap()[0].id.clone();

        sight(State::Watch).unwrap();
        assert_eq!(values(&store, &id).state, State::Watch);
        assert_eq!(
            store.history(&id).unwrap(),
            [Event {
                message: id.clone(),
                user: None,
                at: fixtures::now(),
                text: "Moved to Watch via another client".into(),
            }]
        );

        // A failed event rolls the adoption back with it.
        exec(
            &store,
            "CREATE TRIGGER no_history BEFORE INSERT ON history BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        assert!(matches!(sight(State::Do), Err(Error::Db(_))));
        assert_eq!(values(&store, &id).state, State::Watch);
    }

    #[test]
    fn incoming_reports_a_table_that_refuses_writes() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })
            })
            .unwrap();

        exec(
            &store,
            "CREATE TRIGGER no_threads BEFORE INSERT ON threads BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        assert!(matches!(
            store.import(|tx| tx.incoming("household", &incoming(State::Inbox, None, "x"))),
            Err(Error::Db(_))
        ));
        exec(&store, "DROP TRIGGER no_threads;");

        exec(
            &store,
            "CREATE TRIGGER no_messages BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        assert!(matches!(
            store.import(|tx| tx.incoming("household", &incoming(State::Inbox, None, "x"))),
            Err(Error::Db(_))
        ));
        exec(&store, "DROP TRIGGER no_messages;");
    }

    #[test]
    fn thread_ids_are_scoped_to_their_account() {
        // JMAP thread ids are opaque; two accounts may share one without
        // their threads merging.
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        store
            .import(|tx| {
                tx.account(&Account {
                    slug: "household".into(),
                    name: "Household".into(),
                    address: "household@example.com".into(),
                    read_only: false,
                })?;
                tx.account(&Account {
                    slug: "eli".into(),
                    name: "Eli".into(),
                    address: "eli@example.com".into(),
                    read_only: true,
                })
            })
            .unwrap();
        let mut first = incoming(State::Inbox, None, "ours");
        first.message_id = "m1@chislan.family".into();
        let mut second = incoming(State::Inbox, None, "his");
        second.message_id = "m2@chislan.family".into();
        store
            .import(|tx| {
                tx.incoming("household", &first)?;
                tx.incoming("eli", &second)
            })
            .unwrap();
        let a = store
            .messages(Filter::Search("ours"))
            .unwrap()
            .first()
            .unwrap()
            .thread
            .clone();
        let b = store
            .messages(Filter::Search("his"))
            .unwrap()
            .first()
            .unwrap()
            .thread
            .clone();
        assert_ne!(a, b);
    }

    #[test]
    fn an_ancient_database_migrates_with_its_mail_intact() {
        // A database left at migration 1, holding rows in the tables the
        // second migration rebuilds.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch(MIGRATIONS.first().copied().unwrap())
            .unwrap();
        tx.pragma_update(None, "user_version", 1).unwrap();
        tx.commit().unwrap();
        conn.execute_batch(
            "INSERT INTO users VALUES ('alex@example.com', 'Alex');
             INSERT INTO accounts VALUES ('household', 'Household', 'household@example.com', 0);
             INSERT INTO threads (id, account, subject) VALUES (1, 'household', 'Estimate');
             INSERT INTO messages (id, account, message_id, thread, at, body, kind,
                 from_name, from_addr, state)
                 VALUES (4, 'household', 'm4@northwind.co', 1, '2026-09-27T10:02:00',
                     'Revised estimate.', 'received', 'Northwind', 'office@northwind.co', 'inbox');
             INSERT INTO assignees VALUES (4, 'alex@example.com');
             INSERT INTO reads VALUES ('alex@example.com', 4);",
        )
        .unwrap();

        let store = Store::init(conn, Clock::Fixed(fixtures::now())).unwrap();
        // The mail survived, its ids remade: found by Message-ID now, and
        // the new id is jj-style reverse hex — 16 characters, no digits,
        // no order.
        let msg = store
            .messages(Filter::Search("Revised estimate"))
            .unwrap()
            .first()
            .cloned()
            .unwrap();
        assert_eq!(msg.message_id, "m4@northwind.co");
        assert_eq!(msg.id.len(), 16);
        assert!(msg.id.chars().all(|c| ('k'..='z').contains(&c)));
        assert_eq!(
            msg.values().unwrap().assignees,
            BTreeSet::from([ALEX.to_owned()])
        );
        assert!(!store.unread(ALEX).unwrap().contains(&msg.id));

        // The relaxed schema now takes shared-identity sends.
        store
            .import(|tx| {
                tx.incoming(
                    "household",
                    &Incoming {
                        jmap_id: "E8".into(),
                        message_id: "m8@chislan.family".into(),
                        jmap_thread_id: "T8".into(),
                        subject: "Re: Estimate".into(),
                        at: fixtures::now(),
                        cc: Vec::new(),
                        bcc: Vec::new(),
                        body: "Thanks.".into(),
                        html: None,
                        kind: IncomingKind::Sent {
                            by: None,
                            to: vec!["Northwind".into()],
                        },
                    },
                )
            })
            .unwrap();
        assert!(matches!(
            store
                .messages(Filter::Search("Thanks"))
                .unwrap()
                .first()
                .unwrap()
                .kind,
            Kind::Sent { by: None, .. }
        ));
    }

    #[test]
    fn the_system_clock_reads_real_time() {
        let drift = (Clock::System.now() - jiff::Zoned::now().datetime())
            .total(jiff::Unit::Second)
            .unwrap()
            .abs();
        assert!(drift < 60.0, "system clock drifted {drift}s");
    }

    #[test]
    fn signing_in_adds_users_and_follows_their_slug() {
        let store = Store::open_in_memory(Clock::Fixed(fixtures::now())).unwrap();
        assert!(store.is_empty().unwrap());
        let pat = store.sign_in("pat@example.com", "pat").unwrap();
        assert_eq!(pat, User::new("pat@example.com", "pat"));
        assert!(!store.is_empty().unwrap());
        assert_eq!(store.user("pat@example.com").unwrap(), Some(pat));

        store.sign_in("pat@example.com", "Pat").unwrap();
        store.sign_in("pat@example.com", "Pat").unwrap();
        assert_eq!(store.user("pat@example.com").unwrap().unwrap().slug, "Pat");
        // Slugs are only for show, so two logins may share one.
        store.sign_in("pat@work.example", "Pat").unwrap();
        assert_eq!(store.users().unwrap().len(), 2);
    }

    #[test]
    fn importing_into_a_missing_thread_fails() {
        let store = fixtures::store().unwrap();
        let mut m = store.message(&fixtures::id(4)).unwrap().unwrap();
        m.id = fixtures::id(100);
        m.message_id = "new@example.com".into();
        m.thread = fixtures::id(999);
        let result = store.import(|tx| tx.message(&m));
        assert!(matches!(result, Err(Error::NotFound("thread"))));
        assert!(store.message(&fixtures::id(100)).unwrap().is_none());

        let result = store.import(|tx| tx.html(&fixtures::id(100), "<p>Hi</p>"));
        assert!(matches!(result, Err(Error::NotFound("message"))));
    }

    #[test]
    fn edits_survive_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = camino::Utf8PathBuf::try_from(dir.path().join("docket.db")).unwrap();

        let store = Store::open(&path, Clock::Fixed(fixtures::now())).unwrap();
        assert!(store.is_empty().unwrap());
        fixtures::seed(&store).unwrap();
        assert!(!store.is_empty().unwrap());
        store
            .edit(SAM, &fixtures::id(4), Change::State(State::Wait))
            .unwrap();
        store
            .edit(SAM, &fixtures::id(4), Change::ToggleAssignee(ALEX.into()))
            .unwrap();
        store
            .add_comment(SAM, &fixtures::id(1), "Called them.")
            .unwrap();
        store
            .mark_read(SAM, &fixtures::id(fixtures::WATER))
            .unwrap();
        drop(store);

        let store = Store::open(&path, Clock::Fixed(fixtures::now())).unwrap();
        assert!(!store.is_empty().unwrap());
        let v = values(&store, &fixtures::id(4));
        assert_eq!(v.state, State::Wait);
        assert_eq!(v.assignees, BTreeSet::from([ALEX.to_owned()]));
        assert!(
            store
                .timeline(&fixtures::id(1))
                .unwrap()
                .iter()
                .any(|item| matches!(item, Item::Comment(c) if c.text == "Called them."))
        );
        assert!(
            !store
                .unread(SAM)
                .unwrap()
                .contains(&fixtures::id(fixtures::WATER))
        );
        assert_eq!(store.history(&fixtures::id(4)).unwrap().len(), 2);
        // Undo and toasts belong to the session that made the edit.
        assert!(store.take_flash(SAM).is_none());
        assert!(matches!(store.undo(SAM), Err(Error::BadRequest(_))));
    }

    #[test]
    fn rejects_bad_rows() {
        let store = fixtures::store().unwrap();
        // The schema refuses a received message without a state, and a
        // folder that doesn't exist.
        let id = fixtures::id(4);
        let inner = store.lock();
        assert!(
            inner
                .conn
                .execute(
                    &format!("UPDATE messages SET state = NULL WHERE id = '{id}'"),
                    []
                )
                .is_err()
        );
        assert!(
            inner
                .conn
                .execute(
                    &format!("UPDATE messages SET folder = 'Nope' WHERE id = '{id}'"),
                    []
                )
                .is_err()
        );
        inner
            .conn
            .execute(
                &format!("UPDATE messages SET cc = 'not json' WHERE id = '{id}'"),
                [],
            )
            .unwrap();
        drop(inner);
        assert!(matches!(store.message(&id), Err(Error::Db(_))));
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
            store.edit(SAM, &fixtures::id(4), Change::State(State::Do)),
            Err(Error::Db(_))
        ));
        assert!(matches!(
            store.edit(SAM, &fixtures::id(4), Change::ToggleAssignee(ALEX.into())),
            Err(Error::Db(_))
        ));
        assert_eq!(values(&store, &fixtures::id(4)).state, State::Inbox);
        assert!(values(&store, &fixtures::id(4)).assignees.is_empty());
        assert!(store.history(&fixtures::id(4)).unwrap().is_empty());
        assert!(store.take_flash(SAM).is_none());

        // A failed undo stays undoable.
        exec(&store, "DROP TRIGGER no_history;");
        store
            .edit(SAM, &fixtures::id(4), Change::State(State::Do))
            .unwrap();
        exec(
            &store,
            "CREATE TRIGGER no_history BEFORE INSERT ON history BEGIN SELECT RAISE(ABORT, 'no'); END;",
        );
        assert!(matches!(store.undo(SAM), Err(Error::Db(_))));
        assert_eq!(values(&store, &fixtures::id(4)).state, State::Do);
        exec(&store, "DROP TRIGGER no_history;");
        store.undo(SAM).unwrap();
        assert_eq!(values(&store, &fixtures::id(4)).state, State::Inbox);
    }

    #[test]
    fn database_errors_surface() {
        let store = fixtures::store().unwrap();
        let msg = store.message(&fixtures::id(4)).unwrap().unwrap();
        exec(&store, "PRAGMA query_only = ON;");
        let db_err = |r: Result<(), Error>| assert!(matches!(r, Err(Error::Db(_))), "{r:?}");
        db_err(store.edit(SAM, &fixtures::id(4), Change::State(State::Do)));
        db_err(store.edit(SAM, &fixtures::id(4), Change::ToggleAssignee(ALEX.into())));
        db_err(store.mark_read(SAM, &fixtures::id(4)));
        db_err(store.sign_in("pat@example.com", "pat").map(|_| ()));
        db_err(store.add_comment(SAM, &fixtures::id(1), "hi"));
        db_err(store.import(|tx| tx.folder("Travel")));
        db_err(store.import(|tx| tx.message(&msg)));
        exec(&store, "PRAGMA query_only = OFF;");

        // Each table the store reads, gone in turn.
        exec(&store, "ALTER TABLE folders RENAME TO gone_folders;");
        db_err(store.edit(SAM, &fixtures::id(4), Change::Folder(Some("House".into()))));
        assert!(store.folders().is_err());
        exec(&store, "ALTER TABLE comments RENAME TO gone_comments;");
        assert!(store.timeline(&fixtures::id(1)).is_err());
        exec(&store, "ALTER TABLE reads RENAME TO gone_reads;");
        assert!(store.unread(SAM).is_err());
        exec(&store, "ALTER TABLE history RENAME TO gone_history;");
        assert!(store.history(&fixtures::id(4)).is_err());
        exec(&store, "ALTER TABLE threads RENAME TO gone_threads;");
        db_err(store.add_comment(SAM, &fixtures::id(1), "hi"));
        assert!(store.threads(&BTreeSet::new()).is_err());
        exec(&store, "ALTER TABLE users RENAME TO gone_users;");
        assert!(store.users().is_err());
        assert!(store.is_empty().is_err());
    }
}
