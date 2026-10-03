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

/// Someone Tailscale let in. Tailscale is the whole of authentication, so
/// anyone who reaches Docket is a user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// The `Remote-User` value that identifies this user.
    pub login: String,
    /// The short name shown for them. Not unique: two logins can share one.
    pub slug: String,
}

impl User {
    /// The slug the proxy sent, or the part of the login before the `@`.
    pub fn new(login: &str, slug: Option<&str>) -> Self {
        let fallback = login.split_once('@').map_or(login, |(name, _)| name);
        let slug = slug
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback);
        Self {
            login: login.to_owned(),
            slug: slug.to_owned(),
        }
    }
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
    /// User logins.
    pub assignees: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Received {
        from: String,
        addr: String,
        values: Values,
    },
    /// Sent by one of us (`by` is a login). Sent messages carry no values.
    Sent { by: String, to: Vec<String> },
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: MessageId,
    /// The RFC 5322 Message-ID, without angle brackets.
    pub message_id: String,
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
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub id: CommentId,
    pub thread: ThreadId,
    /// A login.
    pub author: String,
    pub at: DateTime,
    pub text: String,
}

/// A change someone made to a message's values, as its toast put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub message: MessageId,
    /// A login.
    pub user: String,
    pub at: DateTime,
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_fall_back_to_the_login() {
        let named = User::new("alex@example.com", Some(" Al "));
        assert_eq!(named.login, "alex@example.com");
        assert_eq!(named.slug, "Al");
        assert_eq!(User::new("alex@example.com", None).slug, "alex");
        assert_eq!(User::new("alex@example.com", Some("  ")).slug, "alex");
        assert_eq!(User::new("tagged-device", None).slug, "tagged-device");
    }

    #[test]
    fn state_slugs_round_trip() {
        for state in State::ALL {
            assert_eq!(State::from_slug(state.slug()), Some(state));
            assert_eq!(state.to_string(), state.name());
        }
        assert_eq!(State::from_slug("read"), None);
    }
}
