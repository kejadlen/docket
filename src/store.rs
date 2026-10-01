//! An in-memory store seeded from fixtures. It stands in for the database
//! and JMAP sync until those exist; edits last until the process exits.

use std::collections::{BTreeMap, BTreeSet};

use jiff::civil::DateTime;

use crate::Error;
use crate::model::{
    Account, Comment, Kind, Message, MessageId, State, Thread, ThreadId, User, Values,
};

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
#[derive(Debug, Clone, Copy)]
pub enum Item<'a> {
    Message(&'a Message),
    Comment(&'a Comment),
}

#[derive(Debug, Default)]
pub struct Store {
    /// The clock fixtures are written against, so ages stay stable.
    pub now: DateTime,
    pub users: Vec<User>,
    pub accounts: Vec<Account>,
    pub folders: Vec<String>,
    pub threads: Vec<Thread>,
    pub messages: Vec<Message>,
    pub comments: Vec<Comment>,
    /// (user slug, message) pairs that user has read.
    pub read: BTreeSet<(String, MessageId)>,
    undo: BTreeMap<String, (MessageId, Values)>,
    flash: BTreeMap<String, Flash>,
}

impl Store {
    pub fn user(&self, slug: &str) -> Option<&User> {
        self.users.iter().find(|u| u.slug == slug)
    }

    pub fn user_by_login(&self, login: &str) -> Option<&User> {
        self.users.iter().find(|u| u.login == login)
    }

    pub fn user_name<'a>(&'a self, slug: &'a str) -> &'a str {
        self.user(slug).map_or(slug, |u| u.name.as_str())
    }

    pub fn account(&self, slug: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.slug == slug)
    }

    pub fn thread(&self, id: ThreadId) -> Option<&Thread> {
        self.threads.iter().find(|t| t.id == id)
    }

    pub fn message(&self, id: MessageId) -> Option<&Message> {
        self.messages.iter().find(|m| m.id == id)
    }

    pub fn thread_account(&self, thread: ThreadId) -> Option<&Account> {
        self.thread(thread).and_then(|t| self.account(&t.account))
    }

    pub fn thread_messages(&self, thread: ThreadId) -> Vec<&Message> {
        let mut msgs: Vec<_> = self
            .messages
            .iter()
            .filter(|m| m.thread == thread)
            .collect();
        msgs.sort_by_key(|m| (m.at, m.id));
        msgs
    }

    pub fn timeline(&self, thread: ThreadId) -> Vec<Item<'_>> {
        let mut items: Vec<_> = self
            .thread_messages(thread)
            .into_iter()
            .map(Item::Message)
            .chain(
                self.comments
                    .iter()
                    .filter(|c| c.thread == thread)
                    .map(Item::Comment),
            )
            .collect();
        // Stable: a message sorts before a comment made at the same moment.
        items.sort_by_key(|item| match item {
            Item::Message(m) => m.at,
            Item::Comment(c) => c.at,
        });
        items
    }

    pub fn is_unread(&self, user: &str, msg: &Message) -> bool {
        matches!(msg.kind, Kind::Received { .. }) && !self.read.contains(&(user.to_owned(), msg.id))
    }

    pub fn mark_read(&mut self, user: &str, msg: MessageId) {
        self.read.insert((user.to_owned(), msg));
    }

    pub fn edit(&mut self, user: &str, id: MessageId, change: Change) -> Result<(), Error> {
        let msg = self.message(id).ok_or(Error::NotFound("message"))?;
        let read_only = self.thread_account(msg.thread).is_some_and(|a| a.read_only);
        let Kind::Received { values, .. } = &msg.kind else {
            return Err(Error::BadRequest("sent messages have no values"));
        };
        let prev = values.clone();
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
                    Some(f) if !self.folders.contains(&f) => {
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
                let name = self
                    .user(&slug)
                    .ok_or(Error::NotFound("user"))?
                    .name
                    .clone();
                if next.assignees.remove(&slug) {
                    format!("Unassigned {name}")
                } else {
                    next.assignees.insert(slug);
                    format!("Assigned {name}")
                }
            }
        };

        self.set_values(id, next);
        self.undo.insert(user.to_owned(), (id, prev));
        self.flash.insert(
            user.to_owned(),
            Flash {
                text,
                undoable: true,
            },
        );
        Ok(())
    }

    pub fn undo(&mut self, user: &str) -> Result<(), Error> {
        let (id, prev) = self
            .undo
            .remove(user)
            .ok_or(Error::BadRequest("nothing to undo"))?;
        self.set_values(id, prev);
        self.flash.insert(
            user.to_owned(),
            Flash {
                text: "Undone".to_owned(),
                undoable: false,
            },
        );
        Ok(())
    }

    pub fn take_flash(&mut self, user: &str) -> Option<Flash> {
        self.flash.remove(user)
    }

    pub fn add_comment(&mut self, user: &str, thread: ThreadId, text: &str) -> Result<(), Error> {
        self.thread(thread).ok_or(Error::NotFound("thread"))?;
        let text = text.trim();
        if text.is_empty() {
            return Err(Error::BadRequest("comment is empty"));
        }
        let id = self
            .comments
            .iter()
            .map(|c| c.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.comments.push(Comment {
            id,
            thread,
            author: user.to_owned(),
            at: self.now,
            text: text.to_owned(),
        });
        Ok(())
    }

    fn set_values(&mut self, id: MessageId, next: Values) {
        let slot = self.messages.iter_mut().find(|m| m.id == id);
        if let Some(Message {
            kind: Kind::Received { values, .. },
            ..
        }) = slot
        {
            *values = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn values(store: &Store, id: MessageId) -> Values {
        store.message(id).unwrap().values().unwrap().clone()
    }

    #[test]
    fn edits_record_undo_and_flash() {
        let mut store = fixtures::store();
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
    }

    #[test]
    fn folders() {
        let mut store = fixtures::store();
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
    }

    #[test]
    fn read_only_accounts_cannot_file() {
        let mut store = fixtures::store();
        let eli = fixtures::ELI_PRACTICE;
        assert!(matches!(
            store.edit("sam", eli, Change::Folder(None)),
            Err(Error::Forbidden(_))
        ));
        store.edit("sam", eli, Change::State(State::Done)).unwrap();
    }

    #[test]
    fn assignees_toggle() {
        let mut store = fixtures::store();
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
        let mut store = fixtures::store();
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
        let mut store = fixtures::store();
        let before = store.timeline(1).len();
        store.add_comment("sam", 1, "  Called them.  ").unwrap();
        let timeline = store.timeline(1);
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
    fn read_tracking() {
        let mut store = fixtures::store();
        let msg = store.message(fixtures::WATER).unwrap().clone();
        assert!(store.is_unread("alex", &msg));
        store.mark_read("alex", msg.id);
        assert!(!store.is_unread("alex", &msg));
        assert!(store.is_unread("sam", &msg));
        let sent = store.message(2).unwrap().clone();
        assert!(!store.is_unread("sam", &sent));
    }

    #[test]
    fn lookups() {
        let store = fixtures::store();
        assert_eq!(store.user_name("sam"), "Sam");
        assert_eq!(store.user_name("pat"), "pat");
        assert_eq!(
            store.user_by_login("alex@example.com").unwrap().slug,
            "alex"
        );
        assert!(store.user_by_login("nobody").is_none());
    }
}
