//! Which messages a view lists, and how they group. Every list shows
//! individual messages, grouped by thread; a group holds only the thread's
//! messages that belong in that section.

use crate::model::{Kind, Message, State, Thread, User};
use crate::store::Store;

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
pub struct Section<'a> {
    pub head: String,
    /// Compact rows drop the snippet (Watch).
    pub compact: bool,
    pub groups: Vec<Group<'a>>,
}

#[derive(Debug)]
pub struct Group<'a> {
    pub thread: &'a Thread,
    pub rows: Vec<&'a Message>,
}

pub fn sections<'a>(store: &'a Store, me: &User, view: &View) -> Vec<Section<'a>> {
    let received = || store.messages.iter().filter(|m| m.values().is_some());
    let in_state = move |state| received().filter(move |m| m.state() == Some(state));

    let mut out = Vec::new();
    match view {
        View::ForMe => {
            for state in State::LANES {
                let msgs = in_state(state)
                    .filter(|m| {
                        m.is_assigned_to(&me.slug) || (state == State::Inbox && m.is_unassigned())
                    })
                    .collect();
                out.push(section(
                    store,
                    state.name().to_uppercase(),
                    msgs,
                    Order::NewestFirst,
                ));
            }
        }
        View::Lane(state) => {
            let order = if matches!(state, State::Inbox | State::Watch) {
                Order::NewestFirst
            } else {
                Order::OldestFirst
            };
            let mut s = section(store, String::new(), in_state(*state).collect(), order);
            s.compact = *state == State::Watch;
            out.push(s);
        }
        View::Search(query) => {
            let query = query.trim().to_lowercase();
            if !query.is_empty() {
                let hits = store
                    .messages
                    .iter()
                    .filter(|m| matches(store, m, &query))
                    .collect();
                out.push(section(
                    store,
                    "RESULTS".to_owned(),
                    hits,
                    Order::NewestFirst,
                ));
            }
        }
    }
    out.retain(|s| !s.groups.is_empty());
    out
}

/// How many messages a view lists, for the sidebar.
pub fn count(store: &Store, me: &User, view: &View) -> usize {
    sections(store, me, view)
        .iter()
        .flat_map(|s| &s.groups)
        .map(|g| g.rows.len())
        .sum()
}

fn matches(store: &Store, m: &Message, query: &str) -> bool {
    let subject = store.thread(m.thread).map_or("", |t| t.subject.as_str());
    let from = match &m.kind {
        Kind::Received { from, .. } => from.as_str(),
        Kind::Sent { by, .. } => store.user_name(by),
    };
    [subject, from, m.body.as_str()]
        .iter()
        .any(|s| s.to_lowercase().contains(query))
}

fn section<'a>(
    store: &'a Store,
    head: String,
    mut msgs: Vec<&'a Message>,
    order: Order,
) -> Section<'a> {
    msgs.sort_by_key(|m| (m.at, m.id));
    let mut groups: Vec<Group<'a>> = Vec::new();
    for m in msgs {
        if let Some(g) = groups.iter_mut().find(|g| g.thread.id == m.thread) {
            g.rows.push(m);
        } else if let Some(thread) = store.thread(m.thread) {
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
    Section {
        head,
        compact: false,
        groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn summary(store: &Store, user: &str, view: &View) -> Vec<(String, Vec<Vec<u32>>)> {
        let me = store.user(user).unwrap();
        sections(store, me, view)
            .into_iter()
            .map(|s| {
                (
                    s.head,
                    s.groups
                        .iter()
                        .map(|g| g.rows.iter().map(|m| m.id).collect())
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
        let store = fixtures::store();
        let sam = summary(&store, "sam", &View::ForMe);
        assert_eq!(heads(&store, "sam", &View::ForMe), ["INBOX", "DO", "WATCH"]);
        let (_, groups) = &sam[0];
        // Newest thread first; the roofing thread shows only its two Inbox emails.
        assert_eq!(groups[0], vec![fixtures::WATER]);
        assert!(groups.contains(&vec![3, 4]));

        // Alex's own Inbox messages share the section with unassigned ones.
        let alex = summary(&store, "alex", &View::ForMe);
        assert_eq!(heads(&store, "alex", &View::ForMe), ["INBOX", "WAIT"]);
        assert_eq!(
            alex[0].1,
            [vec![7], vec![6], vec![10], vec![8], vec![9], vec![3, 4]]
        );
    }

    #[test]
    fn sidebar_count() {
        let store = fixtures::store();
        let sam = store.user("sam").unwrap();
        assert_eq!(count(&store, sam, &View::ForMe), 7);
    }

    #[test]
    fn lanes() {
        let store = fixtures::store();
        // Every assignee shares one unheaded section: Inbox newest first, Do
        // oldest first.
        let inbox = summary(&store, "sam", &View::Lane(State::Inbox));
        assert_eq!(heads(&store, "sam", &View::Lane(State::Inbox)), [""]);
        assert_eq!(inbox[0].1[0], vec![fixtures::WATER]);
        let todo = summary(&store, "alex", &View::Lane(State::Do));
        assert_eq!(heads(&store, "alex", &View::Lane(State::Do)), [""]);
        assert_eq!(todo[0].1, [vec![13], vec![5]]);
        let wait = summary(&store, "sam", &View::Lane(State::Wait));
        assert_eq!(wait[0].1, vec![vec![11]]);

        let me = store.user("sam").unwrap();
        let watch = sections(&store, me, &View::Lane(State::Watch));
        assert!(watch[0].compact);
        assert_eq!(watch[0].groups[0].rows[0].id, 14);
        assert!(sections(&store, me, &View::Lane(State::Done)).len() == 1);
    }

    #[test]
    fn search_covers_every_message() {
        let store = fixtures::store();
        let hits = summary(&store, "sam", &View::Search("downspout".into()));
        assert_eq!(hits[0].1, vec![vec![2, 4]]);
        let by_sender = summary(&store, "sam", &View::Search("ALEX".into()));
        assert_eq!(by_sender[0].1, vec![vec![12]]);
        let by_subject = summary(&store, "sam", &View::Search("checkup".into()));
        assert_eq!(by_subject[0].1, vec![vec![15]]);
        assert!(summary(&store, "sam", &View::Search("  ".into())).is_empty());
        assert!(summary(&store, "sam", &View::Search("zzz".into())).is_empty());
    }

    #[test]
    fn titles() {
        assert_eq!(View::ForMe.title(), "For me");
        assert_eq!(View::Lane(State::Do).title(), "Do");
        assert_eq!(View::Search("x".into()).title(), "Search");
    }
}
