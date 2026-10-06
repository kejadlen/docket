//! Which messages a view lists, and how they group. Every list shows
//! individual messages, grouped by thread; a group holds only the thread's
//! messages that belong in that section.

use crate::Error;
use crate::model::{Message, State, Thread, User};
use crate::store::{Filter, Store};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    ForMe,
    Lane(State),
    Search(String),
}

impl View {
    pub fn title(&self) -> String {
        match self {
            View::ForMe => "For me".to_owned(),
            View::Lane(state) => state.name().to_owned(),
            View::Search(_) => "Search".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    NewestFirst,
    OldestFirst,
}

#[derive(Debug)]
pub struct Section {
    pub head: String,
    /// Compact rows drop the snippet (Watch).
    pub compact: bool,
    pub groups: Vec<Group>,
}

#[derive(Debug)]
pub struct Group {
    pub thread: Thread,
    pub rows: Vec<Message>,
}

pub fn sections(store: &Store, me: &User, view: &View) -> Result<Vec<Section>, Error> {
    let mut out = Vec::new();
    match view {
        View::ForMe => {
            for state in State::LANES {
                let msgs = store.messages(Filter::ForMe {
                    user: &me.login,
                    state,
                })?;
                let head = state.name().to_uppercase();
                out.push(section(store, head, msgs, Order::NewestFirst)?);
            }
        }
        View::Lane(state) => {
            let order = if matches!(state, State::Inbox | State::Watch) {
                Order::NewestFirst
            } else {
                Order::OldestFirst
            };
            let msgs = store.messages(Filter::State(*state))?;
            let mut s = section(store, String::new(), msgs, order)?;
            s.compact = *state == State::Watch;
            out.push(s);
        }
        View::Search(query) => {
            let query = query.trim();
            if !query.is_empty() {
                let hits = store.messages(Filter::Search(query))?;
                let head = "RESULTS".to_owned();
                out.push(section(store, head, hits, Order::NewestFirst)?);
            }
        }
    }
    out.retain(|s| !s.groups.is_empty());
    Ok(out)
}

/// How many messages a view lists, for the sidebar.
pub fn count(store: &Store, me: &User, view: &View) -> Result<usize, Error> {
    let n = sections(store, me, view)?
        .iter()
        .flat_map(|s| &s.groups)
        .map(|g| g.rows.len())
        .sum();
    Ok(n)
}

/// Takes messages oldest first, as the store returns them.
fn section(
    store: &Store,
    head: String,
    msgs: Vec<Message>,
    order: Order,
) -> Result<Section, Error> {
    let mut groups: Vec<Group> = Vec::new();
    for m in msgs {
        if let Some(g) = groups.iter_mut().find(|g| g.thread.id == m.thread) {
            g.rows.push(m);
        } else if let Some(thread) = store.thread(&m.thread)? {
            groups.push(Group {
                thread,
                rows: vec![m],
            });
        }
    }
    match order {
        Order::OldestFirst => groups.sort_by_key(|g| g.rows.first().map(|m| m.at)),
        Order::NewestFirst => {
            groups.sort_by_key(|g| std::cmp::Reverse(g.rows.last().map(|m| m.at)));
        }
    }
    Ok(Section {
        head,
        compact: false,
        groups,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{self, ALEX, SAM};
    use crate::model::MessageId;

    fn summary(store: &Store, user: &str, view: &View) -> Vec<(String, Vec<Vec<MessageId>>)> {
        let me = store.user(user).unwrap().unwrap();
        sections(store, &me, view)
            .unwrap()
            .into_iter()
            .map(|s| {
                (
                    s.head,
                    s.groups
                        .iter()
                        .map(|g| g.rows.iter().map(|m| m.id.clone()).collect())
                        .collect(),
                )
            })
            .collect()
    }

    fn heads(store: &Store, user: &str, view: &View) -> Vec<String> {
        summary(store, user, view)
            .into_iter()
            .map(|(h, _)| h)
            .collect()
    }

    #[test]
    fn for_me_groups_by_state_then_thread() {
        let store = fixtures::store().unwrap();
        let sam = summary(&store, SAM, &View::ForMe);
        assert_eq!(heads(&store, SAM, &View::ForMe), ["INBOX", "DO", "WATCH"]);
        let (_, groups) = &sam[0];
        // Newest thread first; the roofing thread shows only its two Inbox emails.
        assert_eq!(groups[0], vec![fixtures::id(fixtures::WATER)]);
        assert!(groups.contains(&vec![fixtures::id(3), fixtures::id(4)]));

        // Alex's own Inbox messages share the section with unassigned ones.
        let alex = summary(&store, ALEX, &View::ForMe);
        assert_eq!(heads(&store, ALEX, &View::ForMe), ["INBOX", "WAIT"]);
        assert_eq!(
            alex[0].1,
            [
                vec![fixtures::id(7)],
                vec![fixtures::id(6)],
                vec![fixtures::id(10)],
                vec![fixtures::id(8)],
                vec![fixtures::id(9)],
                vec![fixtures::id(3), fixtures::id(4)]
            ]
        );
    }

    #[test]
    fn sidebar_count() {
        let store = fixtures::store().unwrap();
        let sam = store.user(SAM).unwrap().unwrap();
        assert_eq!(count(&store, &sam, &View::ForMe).unwrap(), 7);
    }

    #[test]
    fn lanes() {
        let store = fixtures::store().unwrap();
        // Every assignee shares one unheaded section: Inbox newest first, Do
        // oldest first.
        let inbox = summary(&store, SAM, &View::Lane(State::Inbox));
        assert_eq!(heads(&store, SAM, &View::Lane(State::Inbox)), [""]);
        assert_eq!(inbox[0].1[0], vec![fixtures::id(fixtures::WATER)]);
        let todo = summary(&store, ALEX, &View::Lane(State::Do));
        assert_eq!(heads(&store, ALEX, &View::Lane(State::Do)), [""]);
        assert_eq!(todo[0].1, [vec![fixtures::id(13)], vec![fixtures::id(5)]]);
        let wait = summary(&store, SAM, &View::Lane(State::Wait));
        assert_eq!(wait[0].1, vec![vec![fixtures::id(11)]]);

        let me = store.user(SAM).unwrap().unwrap();
        let watch = sections(&store, &me, &View::Lane(State::Watch)).unwrap();
        assert!(watch[0].compact);
        assert_eq!(watch[0].groups[0].rows[0].id, fixtures::id(14));
        assert!(
            sections(&store, &me, &View::Lane(State::Done))
                .unwrap()
                .len()
                == 1
        );
    }

    #[test]
    fn search_covers_every_message() {
        let store = fixtures::store().unwrap();
        let hits = summary(&store, SAM, &View::Search("downspout".into()));
        assert_eq!(hits[0].1, vec![vec![fixtures::id(2), fixtures::id(4)]]);
        let by_sender = summary(&store, SAM, &View::Search("ALEX".into()));
        assert_eq!(by_sender[0].1, vec![vec![fixtures::id(12)]]);
        let by_subject = summary(&store, SAM, &View::Search("checkup".into()));
        assert_eq!(by_subject[0].1, vec![vec![fixtures::id(15)]]);
        assert!(summary(&store, SAM, &View::Search("  ".into())).is_empty());
        assert!(summary(&store, SAM, &View::Search("zzz".into())).is_empty());
    }

    #[test]
    fn titles() {
        assert_eq!(View::ForMe.title(), "For me");
        assert_eq!(View::Lane(State::Do).title(), "Do");
        assert_eq!(View::Search("x".into()).title(), "Search");
    }
}
