//! Docket's primitives. A received message carries its own state, folder,
//! and assignees; a thread is only a grouping, and owns nothing but its
//! comments.

use std::collections::BTreeSet;
use std::fmt;

use jiff::civil::DateTime;

pub type MessageId = u32;
pub type ThreadId = u32;
pub type CommentId = u32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    Inbox,
    Do,
    Wait,
    Watch,
    Done,
}

impl State {
    pub const ALL: [State; 5] = [
        State::Inbox,
        State::Do,
        State::Wait,
        State::Watch,
        State::Done,
    ];

    /// The states that have a lane. Done is reached through search only.
    pub const LANES: [State; 4] = [State::Inbox, State::Do, State::Wait, State::Watch];

    pub fn name(self) -> &'static str {
        match self {
            State::Inbox => "Inbox",
            State::Do => "Do",
            State::Wait => "Wait",
            State::Watch => "Watch",
            State::Done => "Done",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            State::Inbox => "inbox",
            State::Do => "do",
            State::Wait => "wait",
            State::Watch => "watch",
            State::Done => "done",
        }
    }

    pub fn from_slug(slug: &str) -> Option<State> {
        State::ALL.into_iter().find(|s| s.slug() == slug)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub slug: String,
    pub name: String,
    /// The `Tailscale-User-Login` value that identifies this user.
    pub login: String,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub slug: String,
    pub name: String,
    pub address: String,
    /// Derived from the token's rights in the real app: no filing or replying.
    pub read_only: bool,
}

#[derive(Debug, Clone)]
pub struct Thread {
    pub id: ThreadId,
    pub account: String,
    pub subject: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Values {
    pub state: State,
    pub folder: Option<String>,
    /// User slugs.
    pub assignees: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Received {
        from: String,
        addr: String,
        values: Values,
    },
    /// Sent by one of us. Sent messages carry no values.
    Sent { by: String, to: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    pub thread: ThreadId,
    pub at: DateTime,
    pub cc: Vec<String>,
    /// Only known for messages we sent.
    pub bcc: Vec<String>,
    pub body: String,
    pub kind: Kind,
}

impl Message {
    pub fn values(&self) -> Option<&Values> {
        match &self.kind {
            Kind::Received { values, .. } => Some(values),
            Kind::Sent { .. } => None,
        }
    }

    pub fn state(&self) -> Option<State> {
        self.values().map(|v| v.state)
    }

    pub fn is_assigned_to(&self, user: &str) -> bool {
        self.values().is_some_and(|v| v.assignees.contains(user))
    }

    pub fn is_unassigned(&self) -> bool {
        self.values().is_some_and(|v| v.assignees.is_empty())
    }
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub id: CommentId,
    pub thread: ThreadId,
    pub author: String,
    pub at: DateTime,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_slugs_round_trip() {
        for state in State::ALL {
            assert_eq!(State::from_slug(state.slug()), Some(state));
            assert_eq!(state.to_string(), state.name());
        }
        assert_eq!(State::from_slug("read"), None);
    }
}
