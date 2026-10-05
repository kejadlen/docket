//! HTML for the three-pane mail layout: mailboxes, the message list, and the
//! thread. Alpine handles the menus and collapsing; every change is a plain
//! form post.

use std::collections::BTreeSet;

use jiff::civil::DateTime;
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::dates;
use crate::lists::{Group, Section, View};
use crate::model::{Account, Comment, Kind, Message, MessageId, State, Thread, User, Values};
use crate::store::{Flash, Item};

/// Everything one page shows, loaded before rendering starts.
pub struct Page<'a> {
    pub me: &'a User,
    pub view: &'a View,
    pub now: DateTime,
    pub users: Vec<User>,
    pub folders: Vec<String>,
    /// The sidebar's views, each with how many messages it lists.
    pub nav: Vec<(View, usize)>,
    pub sections: Vec<Section>,
    /// Received messages `me` hasn't opened.
    pub unread: BTreeSet<MessageId>,
    pub selected: Option<MessageId>,
    /// The selected message's thread.
    pub thread: Option<ThreadView>,
    pub flash: Option<Flash>,
    /// This page's URL, for forms to return to.
    pub here: &'a str,
}

pub struct ThreadView {
    pub thread: Thread,
    /// The thread's account: read-only gates the controls, its name
    /// stands in for the sender of shared-identity mail.
    pub account: Account,
    pub timeline: Vec<Item>,
    /// Opens expanded, along with the selected message.
    pub latest: MessageId,
}

pub fn view_path(view: &View) -> String {
    match view {
        View::ForMe => "/".to_owned(),
        View::Lane(state) => format!("/{}", state.slug()),
        View::Search(query) => format!("/search?q={}", encode(query)),
    }
}

fn select_path(view: &View, id: u32) -> String {
    let path = view_path(view);
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}m={id}")
}

/// Percent-encodes everything but unreserved characters.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Replies happen in Fastmail. Whether its search URLs are a stable way to
/// land on one message is still an open question in DESIGN.md.
pub fn fastmail_url(m: &Message) -> String {
    format!(
        "https://app.fastmail.com/mail/search:{}",
        encode(&format!("msgid:<{}>", m.message_id))
    )
}

/// Gloss Badge tones: Do takes the project accent, Done the fixed success
/// green, everything else stays neutral.
fn tone(state: State) -> &'static str {
    match state {
        State::Do => "accent",
        State::Done => "success",
        State::Inbox | State::Wait | State::Watch => "neutral",
    }
}

fn badge(state: State) -> Markup {
    html! { span.badge.(tone(state)) { span.mark {} (state.name()) } }
}

/// The short name shown for a login, or the login when nobody has it.
fn user_name<'a>(users: &'a [User], login: &'a str) -> &'a str {
    users
        .iter()
        .find(|u| u.login == login)
        .map_or(login, |u| u.slug.as_str())
}

fn assignee_label(users: &[User], values: &Values) -> Option<String> {
    (!values.assignees.is_empty()).then(|| {
        let names: Vec<_> = values
            .assignees
            .iter()
            .map(|a| user_name(users, a).to_uppercase())
            .collect();
        format!("→ {}", names.join(", "))
    })
}

/// Lucide's search, at Gloss's icon weight.
const SEARCH_ICON: &str = r#"<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/></svg>"#;

/// Lucide's external-link, at Gloss's icon weight.
const EXTERNAL_ICON: &str = r#"<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/></svg>"#;

pub fn page(p: &Page<'_>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (p.view.title()) " · Docket" }
                link rel="stylesheet" href="/assets/gloss.css";
                link rel="stylesheet" href="/assets/docket.css";
                script defer src="/assets/alpine.js" {}
            }
            body {
                div.app {
                    (sidebar(p))
                    (list(p))
                    (reader(p))
                }
                span.type-label.version { (crate::VERSION) }
                @if let Some(flash) = &p.flash {
                    (toast(flash, p.here))
                }
            }
        }
    }
}

fn back(here: &str) -> Markup {
    html! { input type="hidden" name="back" value=(here); }
}

fn sidebar(p: &Page<'_>) -> Markup {
    html! {
        nav.side {
            div.brand-row {
                span.brand { "Docket" }
                button.search-btn type="button" aria-label="Search mail" { (PreEscaped(SEARCH_ICON)) }
            }
            div.nav {
                @for (view, count) in &p.nav {
                    a.nav-item.on[view == p.view] href=(view_path(view)) {
                        span { (view.title()) }
                        span.type-figure.count { (count) }
                    }
                }
            }
            div.grow {}
            (whoami(p))
        }
    }
}

fn whoami(p: &Page<'_>) -> Markup {
    let who = html! {
        span.who-name { (p.me.slug) }
        span.type-label.who-login { (p.me.login) }
    };
    html! {
        @if cfg!(feature = "dev") {
            div.who.ctrl x-data="{ open: false }" "@click.outside"="open = false" "@keydown.escape"="open = false" {
                button.who-btn type="button" "@click"="open = !open" { (who) }
                form.menu.up method="post" action="/dev/user" x-show="open" x-cloak {
                    (back(p.here))
                    @for user in &p.users {
                        @let on = user.login == p.me.login;
                        button.opt.cur[on] type="submit" name="user" value=(user.login) {
                            span { (user.slug) }
                            span.check { @if on { "✓" } }
                        }
                    }
                }
            }
        } @else {
            div.who { (who) }
        }
    }
}

fn list(p: &Page<'_>) -> Markup {
    html! {
        section.list aria-label="Messages" {
            header.list-head {
                h1 { (p.view.title()) }
            }
            @if p.sections.is_empty() {
                p.empty { "Nothing here." }
            }
            @for section in &p.sections {
                (list_section(p, section))
            }
        }
    }
}

fn list_section(p: &Page<'_>, section: &Section) -> Markup {
    html! {
        div {
            @if !section.head.is_empty() {
                span.type-label.section-head { (section.head) }
            }
            div.groups {
                @for group in &section.groups {
                    @match group.rows.as_slice() {
                        [m] => (solo(p, group, m, section.compact)),
                        rows => (thread_group(p, group, rows, section.compact)),
                    }
                }
            }
        }
    }
}

/// A thread with several messages listed: the subject heads a row per message.
fn thread_group(p: &Page<'_>, group: &Group, rows: &[Message], compact: bool) -> Markup {
    let unread = rows.iter().any(|m| p.unread.contains(&m.id));
    html! {
        div.group.unread[unread] {
            span.subj { (group.thread.subject) }
            div.rail {
                @for m in rows {
                    (row(p, m, compact))
                }
            }
        }
    }
}

/// A thread with one message listed collapses into a single row: subject and
/// time, then sender and snippet.
fn solo(p: &Page<'_>, group: &Group, m: &Message, compact: bool) -> Markup {
    let selected = p.selected == Some(m.id);
    let unread = p.unread.contains(&m.id);
    html! {
        a.row.solo.sel[selected].unread[unread] href=(select_path(p.view, m.id)) {
            span.subj { (group.thread.subject) }
            span.type-figure.age { (dates::short(p.now, m.at)) }
            span.line {
                span.from { (sender(&p.users, m)) }
                @if !compact {
                    span.snip { " — " (m.body) }
                }
            }
        }
    }
}

fn sender(users: &[User], m: &Message) -> String {
    match &m.kind {
        Kind::Received { from, .. } => from.clone(),
        // A shared-identity send has no login to name; the recipient
        // carries the line.
        Kind::Sent { by, to } => match by.as_deref() {
            Some(by) => format!("{} → {}", user_name(users, by), to.join(", ")),
            None => format!("→ {}", to.join(", ")),
        },
    }
}

fn row(p: &Page<'_>, m: &Message, compact: bool) -> Markup {
    let selected = p.selected == Some(m.id);
    let unread = p.unread.contains(&m.id);
    html! {
        a.row.sel[selected].unread[unread] href=(select_path(p.view, m.id)) {
            span.from { (sender(&p.users, m)) }
            span.type-figure.age { (dates::short(p.now, m.at)) }
            @if !compact {
                span.snip { (m.body) }
            }
        }
    }
}

fn reader(p: &Page<'_>) -> Markup {
    html! {
        main.reader {
            @if let (Some(selected), Some(t)) = (p.selected, &p.thread) {
                (thread(p, t, selected))
            } @else {
                p.empty { "No thread open." }
            }
        }
    }
}

fn thread(p: &Page<'_>, t: &ThreadView, selected: MessageId) -> Markup {
    html! {
        article.thread {
            header.thread-head {
                h2 { (t.thread.subject) }
                @if t.account.read_only {
                    span.type-label { "Read-only" }
                }
            }
            div.chain {
                @for item in &t.timeline {
                    @match item {
                        Item::Comment(c) => (comment(p, c)),
                        Item::Message(m) => {
                            @let open = m.id == selected || m.id == t.latest;
                            (message(p, m, open, m.id == selected, &t.account))
                        }
                    }
                }
            }
            form.comment-box method="post" action=(format!("/threads/{}/comments", t.thread.id)) {
                (back(p.here))
                input.field type="text" name="text" required placeholder="Add an internal comment to this thread"
                    aria-label="Internal comment" autocomplete="off";
                button.btn.primary type="submit" { "Comment" }
            }
        }
    }
}

fn comment(p: &Page<'_>, c: &Comment) -> Markup {
    html! {
        div.item.comment {
            div.body {
                span.author { b { (user_name(&p.users, &c.author)) } }
                span.text { (c.text) }
            }
            div.gutter { span.type-figure.date { (dates::short(p.now, c.at)) } }
        }
    }
}

fn message(p: &Page<'_>, m: &Message, open: bool, selected: bool, account: &Account) -> Markup {
    let (who, addr) = match &m.kind {
        Kind::Received { from, addr, .. } => (from.clone(), format!("<{addr}>")),
        Kind::Sent { by, to } => (
            by.as_deref()
                .map(|by| user_name(&p.users, by).to_owned())
                .unwrap_or_else(|| account.name.clone()),
            format!("→ {}", to.join(", ")),
        ),
    };
    let mut copies = Vec::new();
    if !m.cc.is_empty() {
        copies.push(format!("cc {}", m.cc.join(", ")));
    }
    if !m.bcc.is_empty() {
        copies.push(format!("bcc {}", m.bcc.join(", ")));
    }
    let state = if open {
        "{ open: true }"
    } else {
        "{ open: false }"
    };
    html! {
        div.item.card.sel[selected] id=(format!("m{}", m.id)) x-data=(state) {
            div.body {
                div.from "@click"="open = !open" {
                    b { (who) } " " span.addr { (addr) }
                }
                @if !copies.is_empty() {
                    span.copies { (copies.join(" · ")) }
                }
                span.text x-show="open" x-cloak[!open] { (m.body) }
                span.text.closed x-show="!open" x-cloak[open] "@click"="open = true" { (m.body) }
            }
            div.gutter {
                span.when {
                    span.type-figure.date { (dates::short(p.now, m.at)) }
                    a.icon-btn href=(fastmail_url(m)) target="_blank" rel="noopener"
                        title="Open in Fastmail" aria-label="Open in Fastmail" { (PreEscaped(EXTERNAL_ICON)) }
                }
                @match m.values() {
                    Some(values) => (values_controls(p, m.id, values, account.read_only)),
                    None => span.type-label.sent { "Sent" },
                }
            }
        }
    }
}

/// A value that opens its menu when clicked. Menus open upward, as in the
/// design, unless that would run off the top of the pane.
fn menu(label: Markup, action: String, here: &str, options: Markup) -> Markup {
    html! {
        div.ctrl x-data="{ open: false, up: true }" "@click.outside"="open = false" "@keydown.escape"="open = false" {
            button.ctrl-btn type="button" ":class"="open && 'on'"
                "@click"="up = $el.getBoundingClientRect().top > 260; open = !open" { (label) }
            form.menu method="post" action=(action) x-show="open" x-cloak ":class"="up ? 'up' : 'down'" {
                (back(here))
                (options)
            }
        }
    }
}

fn check(on: bool) -> Markup {
    html! { span.check { @if on { "✓" } } }
}

fn values_controls(p: &Page<'_>, id: u32, values: &Values, read_only: bool) -> Markup {
    let states = html! {
        @for state in State::ALL {
            @let on = state == values.state;
            button.opt.cur[on] type="submit" name="state" value=(state.slug()) {
                (badge(state)) (check(on))
            }
        }
    };
    let folder_label = values.folder.as_deref().unwrap_or("—");
    let folders = html! {
        button.opt.cur[values.folder.is_none()] type="submit" name="folder" value="" {
            span { "No folder" } (check(values.folder.is_none()))
        }
        @for folder in &p.folders {
            @let on = values.folder.as_ref() == Some(folder);
            button.opt.cur[on] type="submit" name="folder" value=(folder) {
                span { (folder) } (check(on))
            }
        }
    };
    let assigned = assignee_label(&p.users, values);
    let people = html! {
        @for user in &p.users {
            @let on = values.assignees.contains(&user.login);
            button.opt.cur[on] type="submit" name="user" value=(user.login) {
                span { (user.slug) } (check(on))
            }
        }
    };
    html! {
        (menu(badge(values.state), format!("/messages/{id}/state"), p.here, states))
        @if read_only {
            span.type-label.value.static { (folder_label) }
        } @else {
            (menu(html! { span.type-label.value { (folder_label) } }, format!("/messages/{id}/folder"), p.here, folders))
        }
        (menu(html! {
            @match &assigned {
                Some(label) => span.type-label.value { (label) },
                None => span.type-label.value.unset { "Assign" },
            }
        }, format!("/messages/{id}/assignees"), p.here, people))
    }
}

fn toast(flash: &Flash, here: &str) -> Markup {
    html! {
        div.toast role="status" x-data="{ show: true }" x-init="setTimeout(() => show = false, 8000)" x-show="show" {
            span.grow { (flash.text) }
            @if flash.undoable {
                form method="post" action="/undo" {
                    (back(here))
                    button.toast-act type="submit" { "UNDO" }
                }
            }
            button.toast-x type="button" aria-label="Dismiss" "@click"="show = false" { (PreEscaped("&times;")) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(view_path(&View::ForMe), "/");
        assert_eq!(view_path(&View::Lane(State::Wait)), "/wait");
        assert_eq!(view_path(&View::Search("a&b".into())), "/search?q=a%26b");
        assert_eq!(select_path(&View::ForMe, 4), "/?m=4");
        assert_eq!(select_path(&View::Search("x".into()), 4), "/search?q=x&m=4");
    }

    #[test]
    fn fastmail_links_search_by_message_id() {
        let store = crate::fixtures::store().unwrap();
        let m = store.message(4).unwrap().unwrap();
        assert_eq!(
            fastmail_url(&m),
            "https://app.fastmail.com/mail/search:msgid%3A%3C4%40fixtures.docket.invalid%3E"
        );
    }

    #[test]
    fn senders_without_a_login_show_the_recipient() {
        let users = [User::new("sam@example.com", "Sam")];
        let sent = |by: Option<&str>| Message {
            id: 1,
            message_id: "m@x".into(),
            thread: 1,
            at: crate::fixtures::now(),
            cc: Vec::new(),
            bcc: Vec::new(),
            body: "hi".into(),
            kind: Kind::Sent {
                by: by.map(str::to_owned),
                to: vec!["Northwind Roofing".into()],
            },
        };
        assert_eq!(
            sender(&users, &sent(Some("sam@example.com"))),
            "Sam → Northwind Roofing"
        );
        // A shared-identity send has nobody to name in a list row.
        assert_eq!(sender(&users, &sent(None)), "→ Northwind Roofing");
    }
}
