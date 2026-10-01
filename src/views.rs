//! HTML for the three-pane mail layout: mailboxes and folders, the message
//! list, and the thread. Alpine handles the menus and collapsing; every
//! change is a plain form post.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::dates;
use crate::lists::{self, Context, Section, View};
use crate::model::{Comment, Kind, Message, State, User, Values};
use crate::store::{Flash, Item, Store};

pub struct Page<'a> {
    pub store: &'a Store,
    pub me: &'a User,
    pub view: &'a View,
    pub selected: Option<&'a Message>,
    pub flash: Option<Flash>,
    /// This page's URL, for forms to return to.
    pub here: &'a str,
    /// Lets the viewer switch users without Tailscale, for fixtures.
    pub dev: bool,
}

pub fn view_path(view: &View) -> String {
    match view {
        View::ForMe => "/".to_owned(),
        View::Lane(state) => format!("/{}", state.slug()),
        View::Folder(folder) => format!("/folders/{}", encode(folder)),
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

fn state_mark(state: State) -> &'static str {
    match state {
        State::Inbox => "var(--gl-color-text-tertiary)",
        State::Do => "var(--gl-color-text-primary)",
        State::Wait => "var(--gl-color-text-secondary)",
        State::Watch => "var(--gl-color-accent)",
        State::Done => "var(--gl-color-success)",
    }
}

fn assignee_label(store: &Store, values: &Values) -> Option<String> {
    (!values.assignees.is_empty()).then(|| {
        let names: Vec<_> = values
            .assignees
            .iter()
            .map(|a| store.user_name(a).to_uppercase())
            .collect();
        format!("→ {}", names.join(", "))
    })
}

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
    let query = match p.view {
        View::Search(q) => q.as_str(),
        _ => "",
    };
    let views = std::iter::once(View::ForMe).chain(State::LANES.map(View::Lane));
    html! {
        nav.side {
            form.search action="/search" method="get" {
                input type="search" name="q" placeholder="Search mail" value=(query) aria-label="Search mail";
            }
            @for view in views {
                a.nav-item.on[&view == p.view] href=(view_path(&view)) {
                    span { (view.title()) }
                    span.count { (lists::count(p.store, p.me, &view)) }
                }
            }
            div.side-head { "FOLDERS" }
            @for folder in &p.store.folders {
                @let view = View::Folder(folder.clone());
                a.nav-item.on[&view == p.view] href=(view_path(&view)) { span { (folder) } }
            }
            div.grow {}
            (whoami(p))
        }
    }
}

fn whoami(p: &Page<'_>) -> Markup {
    let who = html! {
        span.who-name { (p.me.name) }
        span.who-login { (p.me.login) }
    };
    html! {
        @if p.dev {
            div.who.ctrl x-data="{ open: false }" "@click.outside"="open = false" "@keydown.escape"="open = false" {
                button.who-btn type="button" "@click"="open = !open" { (who) }
                form.menu.menu-up method="post" action="/dev/user" x-show="open" x-cloak {
                    (back(p.here))
                    @for user in &p.store.users {
                        button.opt.cur[user.slug == p.me.slug] type="submit" name="user" value=(user.slug) {
                            span.box.on[user.slug == p.me.slug] {}
                            (user.name)
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
    let sections = lists::sections(p.store, p.me, p.view);
    let total: usize = sections
        .iter()
        .flat_map(|s| &s.groups)
        .map(|g| g.rows.len())
        .sum();
    html! {
        section.list aria-label="Messages" {
            header.list-head {
                h1 { (p.view.title()) }
                span.mono { (total) }
            }
            @if sections.is_empty() {
                p.empty { "Nothing here." }
            }
            @for section in &sections {
                (list_section(p, section))
            }
        }
    }
}

fn list_section(p: &Page<'_>, section: &Section<'_>) -> Markup {
    html! {
        @if !section.head.is_empty() {
            div.section-head {
                span { (section.head) }
                span.sub { (section.sub) }
            }
        }
        @for group in &section.groups {
            div.group {
                div.group-head {
                    span.subj { (group.thread.subject) }
                    @if let Some(more) = group.more() {
                        span.more { (more) }
                    }
                }
                @for m in &group.rows {
                    (row(p, m, &section.context, section.compact))
                }
            }
        }
    }
}

fn sender(store: &Store, m: &Message) -> String {
    match &m.kind {
        Kind::Received { from, .. } => from.clone(),
        Kind::Sent { by, to } => format!("{} → {}", store.user_name(by), to.join(", ")),
    }
}

fn row_meta(store: &Store, m: &Message, context: &Context) -> String {
    let Some(values) = m.values() else {
        return String::new();
    };
    let mut parts = Vec::new();
    if context.show_state {
        parts.push(values.state.name().to_uppercase());
    }
    if context.show_folder
        && let Some(folder) = &values.folder
    {
        parts.push(folder.to_uppercase());
    }
    if context.implied.as_ref() != Some(&values.assignees)
        && let Some(label) = assignee_label(store, values)
    {
        parts.push(label);
    }
    parts.join(" · ")
}

fn row(p: &Page<'_>, m: &Message, context: &Context, compact: bool) -> Markup {
    let selected = p.selected.is_some_and(|s| s.id == m.id);
    let unread = p.store.is_unread(&p.me.slug, m);
    let meta = row_meta(p.store, m, context);
    html! {
        a.row.sel[selected].unread[unread] href=(select_path(p.view, m.id)) {
            span.dot {}
            span.from { (sender(p.store, m)) }
            span.age { (dates::short(p.store.now, m.at)) }
            @if !compact {
                span.snip { (m.body) }
            }
            @if !meta.is_empty() {
                span.meta { (meta) }
            }
        }
    }
}

fn reader(p: &Page<'_>) -> Markup {
    html! {
        main.reader {
            @if let Some(selected) = p.selected {
                (thread(p, selected))
            } @else {
                p.empty { "No thread open." }
            }
        }
    }
}

fn thread(p: &Page<'_>, selected: &Message) -> Markup {
    let store = p.store;
    let subject = store
        .thread(selected.thread)
        .map_or("", |t| t.subject.as_str());
    let account = store.thread_account(selected.thread);
    let read_only = account.is_some_and(|a| a.read_only);
    let msgs = store.thread_messages(selected.thread);
    let latest = msgs.last().map(|m| m.id);
    let count = match msgs.len() {
        1 => "1 MESSAGE".to_owned(),
        n => format!("{n} MESSAGES"),
    };
    let mut meta = vec![account.map_or(String::new(), |a| a.name.to_uppercase())];
    if read_only {
        meta.push("READ-ONLY".to_owned());
    }
    meta.push(count);
    html! {
        article.thread {
            header.thread-head {
                h2 { (subject) }
                span.mono { (meta.join(" · ")) }
            }
            @for item in store.timeline(selected.thread) {
                @match item {
                    Item::Comment(c) => (comment(store, c)),
                    Item::Message(m) => {
                        @let open = m.id == selected.id || Some(m.id) == latest;
                        (message(p, m, open, m.id == selected.id, read_only))
                    }
                }
            }
            form.comment-box method="post" action=(format!("/threads/{}/comments", selected.thread))
                x-data="{ text: '' }" {
                (back(p.here))
                textarea name="text" rows="1" placeholder="Add an internal comment to this thread"
                    aria-label="Internal comment" x-model="text"
                    "@keydown.enter"="if (!$event.shiftKey) { $event.preventDefault(); if (text.trim()) $el.form.requestSubmit() }" {}
                button.btn type="submit" x-show="text.trim()" x-cloak { "COMMENT" }
            }
        }
    }
}

fn comment(store: &Store, c: &Comment) -> Markup {
    html! {
        div.item.comment {
            div.body {
                b { (store.user_name(&c.author)) }
                span.text { (c.text) }
            }
            div.gutter { span.date { (dates::short(store.now, c.at)) } }
        }
    }
}

fn message(p: &Page<'_>, m: &Message, open: bool, selected: bool, read_only: bool) -> Markup {
    let store = p.store;
    let (who, addr) = match &m.kind {
        Kind::Received { from, addr, .. } => (from.clone(), format!("<{addr}>")),
        Kind::Sent { by, to } => (
            store.user_name(by).to_owned(),
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
        div.item.sel[selected] id=(format!("m{}", m.id)) x-data=(state) {
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
                span.date { (dates::short(store.now, m.at)) }
                @if let Some(values) = m.values() {
                    (values_controls(p, m.id, values, read_only))
                }
            }
        }
    }
}

fn menu(label: Markup, action: String, here: &str, options: Markup) -> Markup {
    html! {
        div.ctrl x-data="{ open: false }" "@click.outside"="open = false" "@keydown.escape"="open = false" {
            button.ctrl-btn type="button" "@click"="open = !open" ":class"="open && 'on'" { (label) }
            form.menu method="post" action=(action) x-show="open" x-cloak {
                (back(here))
                (options)
            }
        }
    }
}

fn values_controls(p: &Page<'_>, id: u32, values: &Values, read_only: bool) -> Markup {
    let state_label = html! {
        span.sq style=(format!("background:{}", state_mark(values.state))) {}
        (values.state.name().to_uppercase())
    };
    let states = html! {
        @for state in State::ALL {
            button.opt.cur[state == values.state] type="submit" name="state" value=(state.slug()) {
                span.box style=(format!("background:{}", state_mark(state))) {}
                (state.name())
            }
        }
    };
    let folder_label = html! {
        span.sq {}
        (values.folder.as_deref().map_or("—".to_owned(), str::to_uppercase))
    };
    let folders = html! {
        button.opt.cur[values.folder.is_none()] type="submit" name="folder" value="" {
            span.box.on[values.folder.is_none()] {} "No folder"
        }
        @for folder in &p.store.folders {
            @let on = values.folder.as_ref() == Some(folder);
            button.opt.cur[on] type="submit" name="folder" value=(folder) {
                span.box.on[on] {} (folder)
            }
        }
    };
    let assigned_label = html! {
        span.sq {}
        (assignee_label(p.store, values).unwrap_or_else(|| "ASSIGN".to_owned()))
    };
    let people = html! {
        @for user in &p.store.users {
            @let on = values.assignees.contains(&user.slug);
            button.opt.cur[on] type="submit" name="user" value=(user.slug) {
                span.box.on[on] {} (user.name)
            }
        }
    };
    html! {
        (menu(state_label, format!("/messages/{id}/state"), p.here, states))
        @if read_only {
            span.ctrl-static { (folder_label) }
        } @else {
            (menu(folder_label, format!("/messages/{id}/folder"), p.here, folders))
        }
        (menu(assigned_label, format!("/messages/{id}/assignees"), p.here, people))
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
        assert_eq!(
            view_path(&View::Folder("Kid stuff".into())),
            "/folders/Kid%20stuff"
        );
        assert_eq!(view_path(&View::Search("a&b".into())), "/search?q=a%26b");
        assert_eq!(select_path(&View::ForMe, 4), "/?m=4");
        assert_eq!(select_path(&View::Search("x".into()), 4), "/search?q=x&m=4");
    }
}
