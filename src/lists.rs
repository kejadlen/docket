//! Which messages a view lists, and how they group. Every list shows
//! individual messages, grouped by thread; a group holds only the thread's
//! messages that belong in that section.

use std::collections::BTreeSet;

use crate::model::{Kind, Message, State, Thread, User};
use crate::store::Store;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    ForMe,
    Lane(State),
    Folder(String),
    Search(String),
}

impl View {
    pub fn title(&self) -> String {
        match self {
            View::ForMe => "For me".to_owned(),
            View::Lane(state) => state.name().to_owned(),
            View::Folder(folder) => folder.clone(),
            View::Search(_) => "Search".to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    NewestFirst,
    OldestFirst,
}

/// What a section already says about its rows, so rows needn't repeat it.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub show_state: bool,
    pub show_folder: bool,
    /// Rows assigned to exactly these people leave assignees off.
    pub implied: Option<BTreeSet<String>>,
}

#[derive(Debug)]
pub struct Section<'a> {
    pub head: String,
    pub sub: String,
    /// Compact rows drop the snippet (Watch).
    pub compact: bool,
    pub context: Context,
    pub groups: Vec<Group<'a>>,
}

#[derive(Debug)]
pub struct Group<'a> {
    pub thread: &'a Thread,
    /// All messages in the thread, sent ones included.
    pub total: usize,
    pub rows: Vec<&'a Message>,
}

impl Group<'_> {
    /// "2 OF 4" when the group shows only part of its thread.
    pub fn more(&self) -> Option<String> {
        (self.rows.len() < self.total).then(|| format!("{} OF {}", self.rows.len(), self.total))
    }
}

pub fn sections<'a>(store: &'a Store, me: &User, view: &View) -> Vec<Section<'a>> {
    let received = || store.messages.iter().filter(|m| m.values().is_some());
    let in_state = move |state| received().filter(move |m| m.state() == Some(state));
    let me_only: BTreeSet<String> = BTreeSet::from([me.slug.clone()]);
    let to_me = format!("→ {}", me.name.to_uppercase());
    let them: Vec<_> = store
        .users
        .iter()
        .filter(|u| u.slug != me.slug)
        .map(|u| u.name.to_uppercase())
        .collect();
    let to_them = format!("→ {}", them.join(", "));
    let folder_context = Context {
        show_folder: true,
        ..Context::default()
    };

    let mut out = Vec::new();
    match view {
        View::ForMe => {
            for state in State::LANES {
                let mine = in_state(state).filter(|m| m.is_assigned_to(&me.slug));
                out.push(section(
                    store,
                    (state.name().to_uppercase(), to_me.clone()),
                    Context {
                        implied: Some(me_only.clone()),
                        ..folder_context.clone()
                    },
                    mine.collect(),
                    Order::NewestFirst,
                ));
                if state == State::Inbox {
                    out.push(section(
                        store,
                        (state.name().to_uppercase(), "NO ONE’S".to_owned()),
                        folder_context.clone(),
                        in_state(state).filter(|m| m.is_unassigned()).collect(),
                        Order::NewestFirst,
                    ));
                }
            }
        }
        View::Lane(state @ (State::Inbox | State::Do)) => {
            let order = if *state == State::Do {
                Order::OldestFirst
            } else {
                Order::NewestFirst
            };
            let mine = (to_me, String::new());
            let nobody = ("NO ONE’S".to_owned(), String::new());
            let theirs = (to_them, String::new());
            let mine_msgs = in_state(*state)
                .filter(|m| m.is_assigned_to(&me.slug))
                .collect();
            let nobody_msgs = in_state(*state).filter(|m| m.is_unassigned()).collect();
            let theirs_msgs = in_state(*state)
                .filter(|m| !m.is_unassigned() && !m.is_assigned_to(&me.slug))
                .collect();
            let mine = section(
                store,
                mine,
                Context {
                    implied: Some(me_only),
                    ..folder_context.clone()
                },
                mine_msgs,
                order,
            );
            let nobody = section(store, nobody, folder_context.clone(), nobody_msgs, order);
            let theirs = section(store, theirs, folder_context, theirs_msgs, order);
            // Inbox reads me / them / no one; Do reads mine / unassigned / theirs.
            if *state == State::Do {
                out.extend([mine, nobody, theirs]);
            } else {
                out.extend([mine, theirs, nobody]);
            }
        }
        View::Lane(state) => {
            let order = if *state == State::Watch {
                Order::NewestFirst
            } else {
                Order::OldestFirst
            };
            let mut s = section(
                store,
                (String::new(), String::new()),
                folder_context,
                in_state(*state).collect(),
                order,
            );
            s.compact = *state == State::Watch;
            out.push(s);
        }
        View::Folder(folder) => {
            for state in State::ALL {
                out.push(section(
                    store,
                    (state.name().to_uppercase(), String::new()),
                    Context::default(),
                    in_state(state)
                        .filter(|m| m.values().and_then(|v| v.folder.as_ref()) == Some(folder))
                        .collect(),
                    if state == State::Done {
                        Order::NewestFirst
                    } else {
                        Order::OldestFirst
                    },
                ));
            }
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
                    ("RESULTS".to_owned(), String::new()),
                    Context {
                        show_state: true,
                        ..folder_context
                    },
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
    (head, sub): (String, String),
    context: Context,
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
                total: store.thread_messages(thread.id).len(),
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
        sub,
        compact: false,
        context,
        groups,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn summary(store: &Store, user: &str, view: &View) -> Vec<(String, String, Vec<Vec<u32>>)> {
        let me = store.user(user).unwrap();
        sections(store, me, view)
            .into_iter()
            .map(|s| {
                (
                    s.head,
                    s.sub,
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
            .map(|(h, s, _)| format!("{h}|{s}"))
            .collect()
    }

    #[test]
    fn for_me_groups_by_state_then_thread() {
        let store = fixtures::store();
        let sam = summary(&store, "sam", &View::ForMe);
        assert_eq!(
            heads(&store, "sam", &View::ForMe),
            ["INBOX|NO ONE’S", "DO|→ SAM", "WATCH|→ SAM"]
        );
        let (_, _, groups) = &sam[0];
        // Newest thread first; the roofing thread shows only its two Inbox emails.
        assert_eq!(groups[0], vec![fixtures::WATER]);
        assert!(groups.contains(&vec![3, 4]));

        assert_eq!(
            heads(&store, "alex", &View::ForMe),
            ["INBOX|→ ALEX", "INBOX|NO ONE’S", "WAIT|→ ALEX"]
        );
    }

    #[test]
    fn group_counts() {
        let store = fixtures::store();
        let sam = store.user("sam").unwrap();
        let sections = sections(&store, sam, &View::ForMe);
        let roofing = sections[0]
            .groups
            .iter()
            .find(|g| g.thread.id == 1)
            .unwrap();
        assert_eq!(roofing.more().as_deref(), Some("2 OF 4"));
        let water = &sections[0].groups[0];
        assert_eq!(water.more(), None);
        assert_eq!(count(&store, sam, &View::ForMe), 7);
    }

    #[test]
    fn lanes() {
        let store = fixtures::store();
        assert_eq!(
            heads(&store, "sam", &View::Lane(State::Inbox)),
            ["→ ALEX|", "NO ONE’S|"]
        );
        assert_eq!(
            heads(&store, "alex", &View::Lane(State::Inbox)),
            ["→ ALEX|", "NO ONE’S|"]
        );
        assert_eq!(
            heads(&store, "alex", &View::Lane(State::Do)),
            ["NO ONE’S|", "→ SAM|"]
        );
        assert_eq!(
            heads(&store, "sam", &View::Lane(State::Do)),
            ["→ SAM|", "NO ONE’S|"]
        );
        let wait = summary(&store, "sam", &View::Lane(State::Wait));
        assert_eq!(wait[0].2, vec![vec![11]]);

        let me = store.user("sam").unwrap();
        let watch = sections(&store, me, &View::Lane(State::Watch));
        assert!(watch[0].compact);
        assert_eq!(watch[0].groups[0].rows[0].id, 14);
        assert!(sections(&store, me, &View::Lane(State::Done)).len() == 1);
    }

    #[test]
    fn do_lane_orders_mine_unassigned_theirs() {
        let mut store = fixtures::store();
        store
            .edit("alex", 3, crate::store::Change::State(State::Do))
            .unwrap();
        store
            .edit(
                "alex",
                3,
                crate::store::Change::ToggleAssignee("alex".into()),
            )
            .unwrap();
        assert_eq!(
            heads(&store, "alex", &View::Lane(State::Do)),
            ["→ ALEX|", "NO ONE’S|", "→ SAM|"]
        );
        // Inbox reads me / them / no one.
        store
            .edit(
                "alex",
                4,
                crate::store::Change::ToggleAssignee("sam".into()),
            )
            .unwrap();
        assert_eq!(
            heads(&store, "alex", &View::Lane(State::Inbox)),
            ["→ ALEX|", "→ SAM|", "NO ONE’S|"]
        );
    }

    #[test]
    fn folders_group_by_every_state() {
        let store = fixtures::store();
        assert_eq!(
            heads(&store, "sam", &View::Folder("House".into())),
            ["INBOX|", "WAIT|", "WATCH|", "DONE|"]
        );
        assert_eq!(
            heads(&store, "sam", &View::Folder("Medical".into())),
            ["INBOX|", "DONE|"]
        );
    }

    #[test]
    fn search_covers_every_message() {
        let store = fixtures::store();
        let hits = summary(&store, "sam", &View::Search("downspout".into()));
        assert_eq!(hits[0].2, vec![vec![2, 4]]);
        let by_sender = summary(&store, "sam", &View::Search("ALEX".into()));
        assert_eq!(by_sender[0].2, vec![vec![12]]);
        let by_subject = summary(&store, "sam", &View::Search("checkup".into()));
        assert_eq!(by_subject[0].2, vec![vec![15]]);
        assert!(summary(&store, "sam", &View::Search("  ".into())).is_empty());
        assert!(summary(&store, "sam", &View::Search("zzz".into())).is_empty());
    }

    #[test]
    fn titles() {
        assert_eq!(View::ForMe.title(), "For me");
        assert_eq!(View::Lane(State::Do).title(), "Do");
        assert_eq!(View::Folder("House".into()).title(), "House");
        assert_eq!(View::Search("x".into()).title(), "Search");
    }
}
