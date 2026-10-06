//! Message bodies as the thread view shows them: plain text, escaped,
//! with its URLs made into links and the quoted history it trails split
//! off to hide — and the one-line snippet a list row shows.

use maud::{Markup, html};

/// A run of a body: plain text, or a URL to link.
#[derive(Debug, PartialEq, Eq)]
enum Segment<'a> {
    Text(&'a str),
    Url(&'a str),
}

/// Splits text into plain runs and the http(s) URLs between them. A URL
/// starts at its scheme, after a non-alphanumeric boundary, and runs to
/// whitespace or a character that can't appear unescaped in one; trailing
/// sentence punctuation and unbalanced closing brackets stay text.
fn segments(text: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut plain = 0;
    let mut at = 0;
    while let Some(found) = next_scheme(text, at) {
        let rest = &text[found..];
        let end = rest
            .find(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"'))
            .unwrap_or(rest.len());
        let url = trim_url(&rest[..end]);
        if url.ends_with("//") {
            // A bare scheme links nowhere.
            at = found.saturating_add(end);
            continue;
        }
        if plain < found {
            out.push(Segment::Text(&text[plain..found]));
        }
        out.push(Segment::Url(url));
        plain = found.saturating_add(url.len());
        at = plain;
    }
    if plain < text.len() {
        out.push(Segment::Text(&text[plain..]));
    }
    out
}

/// The byte offset of the next `http://` or `https://` at or after `from`
/// that starts a word.
fn next_scheme(text: &str, from: usize) -> Option<usize> {
    let mut at = from;
    loop {
        let found = at.saturating_add(text[at..].find("http")?);
        let rest = &text[found..];
        let starts_word = !text[..found]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric);
        if starts_word && (rest.starts_with("http://") || rest.starts_with("https://")) {
            return Some(found);
        }
        at = found.saturating_add("http".len());
    }
}

/// Drops what reads as the sentence around a URL rather than the URL:
/// trailing punctuation, and closing brackets the URL never opened.
fn trim_url(mut url: &str) -> &str {
    let trims = |url: &str, last: char| {
        let unbalanced =
            |open: char, close: char| url.matches(close).count() > url.matches(open).count();
        match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' | '…' => true,
            ')' => unbalanced('(', ')'),
            ']' => unbalanced('[', ']'),
            _ => false,
        }
    };
    while let Some((cut, _)) = url
        .char_indices()
        .next_back()
        .filter(|&(_, last)| trims(url, last))
    {
        url = &url[..cut];
    }
    url
}

/// The text, escaped, with each URL a link that opens in a new tab.
pub fn linked(text: &str) -> Markup {
    html! {
        @for segment in segments(text) {
            @match segment {
                Segment::Text(t) => (t),
                Segment::Url(u) => a href=(u) target="_blank" rel="noopener" { (u) },
            }
        }
    }
}

/// How much of a body a list row carries: more than a row ever shows, so
/// CSS still does the clipping, without shipping the whole body.
const SNIPPET: usize = 160;

/// The start of what a body says, on one line, for a list row.
pub fn snippet(text: &str) -> String {
    let (said, _) = split_quoted(text);
    said.split_whitespace()
        .flat_map(|word| [" ", word])
        .skip(1)
        .flat_map(str::chars)
        .take(SNIPPET)
        .collect()
}

/// Splits a reply into what it says and the quoted history it trails:
/// from an Outlook-style separator ("-----Original Message-----", or a
/// rule over a "From:" line) to the end, or else a closing run of `>`
/// lines with the "On …, … wrote:" attribution above it. Only a trailing
/// quote goes — inline replies need the lines they answer — and a body
/// that is all quote keeps it, so nothing ever renders empty.
pub fn split_quoted(text: &str) -> (&str, Option<&str>) {
    let mut at = 0usize;
    let lines: Vec<(usize, &str)> = text
        .split_inclusive('\n')
        .map(|line| {
            let start = at;
            at = at.saturating_add(line.len());
            (start, line.trim())
        })
        .collect();
    let Some(start) = separator(&lines).or_else(|| quote_tail(&lines)) else {
        return (text, None);
    };
    let said = text[..start].trim_end();
    if said.is_empty() {
        return (text, None);
    }
    (said, Some(text[start..].trim()))
}

/// Where an Outlook-style separator starts the quoted history.
fn separator(lines: &[(usize, &str)]) -> Option<usize> {
    let next = lines.iter().skip(1).map(Some).chain([None]);
    lines
        .iter()
        .zip(next)
        .find(|((_, line), next)| {
            let original =
                line.starts_with("-----") && line.to_lowercase().contains("original message");
            let rule = line.len() >= 10 && line.chars().all(|c| c == '_');
            original || (rule && next.is_some_and(|(_, n)| n.starts_with("From:")))
        })
        .map(|((at, _), _)| *at)
}

/// Where a closing run of `>` lines starts, taking in the attribution
/// line above it — wrapped onto two lines if need be.
fn quote_tail(lines: &[(usize, &str)]) -> Option<usize> {
    let quoted = |line: &str| line.is_empty() || line.starts_with('>');
    let first = lines
        .iter()
        .rposition(|(_, line)| !quoted(line))
        .map_or(0, |i| i.saturating_add(1));
    if !lines
        .iter()
        .skip(first)
        .any(|(_, line)| line.starts_with('>'))
    {
        return None;
    }
    // The line above index i, with its index.
    let above = |i: usize| {
        let j = i.checked_sub(1)?;
        lines.get(j).map(|(_, line)| (j, *line))
    };
    let mut start = first;
    if let Some((j, wrote)) = above(first).filter(|(_, line)| line.ends_with("wrote:")) {
        start = j;
        // A wrapped attribution's first line opens with "On" and a date.
        let opens = |on: &str| on.starts_with("On ") && on.contains(|c: char| c.is_ascii_digit());
        if let Some((k, _)) = above(j).filter(|(_, on)| !wrote.starts_with("On ") && opens(on)) {
            start = k;
        }
    }
    lines.get(start).map(|(at, _)| *at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Segment::{Text, Url};

    #[test]
    fn urls_split_out_of_the_text() {
        assert_eq!(
            segments("See https://example.com/a?b=1&c=2 for details"),
            [
                Text("See "),
                Url("https://example.com/a?b=1&c=2"),
                Text(" for details")
            ]
        );
        assert_eq!(segments("http://x.org"), [Url("http://x.org")]);
        assert_eq!(segments("no links here"), [Text("no links here")]);
        assert_eq!(segments(""), []);
        // Angle brackets and quotes end a URL.
        assert_eq!(
            segments("<https://a.com/x>"),
            [Text("<"), Url("https://a.com/x"), Text(">")]
        );
        assert_eq!(
            segments("one https://a.com\ntwo https://b.com"),
            [
                Text("one "),
                Url("https://a.com"),
                Text("\ntwo "),
                Url("https://b.com")
            ]
        );
    }

    #[test]
    fn sentence_punctuation_stays_text() {
        assert_eq!(
            segments("Go to https://a.com/x."),
            [Text("Go to "), Url("https://a.com/x"), Text(".")]
        );
        assert_eq!(
            segments("(see https://a.com/x), then"),
            [Text("(see "), Url("https://a.com/x"), Text("), then")]
        );
        // Brackets the URL opened are part of it.
        assert_eq!(
            segments("https://en.wikipedia.org/wiki/Rust_(language)!"),
            [
                Url("https://en.wikipedia.org/wiki/Rust_(language)"),
                Text("!")
            ]
        );
        assert_eq!(
            segments("[https://a.com/x[1]]"),
            [Text("["), Url("https://a.com/x[1]"), Text("]")]
        );
    }

    #[test]
    fn only_whole_schemes_at_a_word_start_link() {
        assert_eq!(segments("xhttps://a.com"), [Text("xhttps://a.com")]);
        assert_eq!(
            segments("httpd and https:// alone"),
            [Text("httpd and https:// alone")]
        );
        assert_eq!(segments("https://."), [Text("https://.")]);
        // Non-ASCII around a URL keeps the byte offsets honest.
        assert_eq!(
            segments("café→https://a.com/é…"),
            [Text("café→"), Url("https://a.com/é"), Text("…")]
        );
    }

    #[test]
    fn links_open_in_a_new_tab_and_text_stays_escaped() {
        assert_eq!(
            linked("<b> https://a.com/?x=1&y=\"2\"").into_string(),
            r#"&lt;b&gt; <a href="https://a.com/?x=1&amp;y=" target="_blank" rel="noopener">https://a.com/?x=1&amp;y=</a>&quot;2&quot;"#
        );
    }

    #[test]
    fn a_trailing_quote_splits_off_with_its_attribution() {
        assert_eq!(
            split_quoted("Sounds good.\n\nOn Mon, Oct 5, Sam wrote:\n> Thursday?\n>\n> -S\n"),
            (
                "Sounds good.",
                Some("On Mon, Oct 5, Sam wrote:\n> Thursday?\n>\n> -S")
            )
        );
        // An attribution wrapped onto two lines comes along whole.
        assert_eq!(
            split_quoted(
                "Yes.\nOn Mon, Oct 5, 2026 at 9:41 AM\nSam <sam@example.com> wrote:\n> Thursday?"
            ),
            (
                "Yes.",
                Some("On Mon, Oct 5, 2026 at 9:41 AM\nSam <sam@example.com> wrote:\n> Thursday?")
            )
        );
        // A bare quote with no attribution, CRLF line ends.
        assert_eq!(
            split_quoted("Yes.\r\n> Thursday?\r\n"),
            ("Yes.", Some("> Thursday?"))
        );
        // A "wrote:" line on its own is attribution enough; the line
        // above it stays when it doesn't open one.
        assert_eq!(
            split_quoted("Yes.\nSam wrote:\n> Thursday?"),
            ("Yes.", Some("Sam wrote:\n> Thursday?"))
        );
        assert_eq!(
            split_quoted("On it.\nI wrote:\n> Thursday?"),
            ("On it.", Some("I wrote:\n> Thursday?"))
        );
    }

    #[test]
    fn outlook_separators_quote_everything_after() {
        assert_eq!(
            split_quoted("Approved.\n\n-----Original Message-----\nFrom: Pat\nPlease approve."),
            (
                "Approved.",
                Some("-----Original Message-----\nFrom: Pat\nPlease approve.")
            )
        );
        assert_eq!(
            split_quoted("Approved.\n________________________________\nFrom: Pat\nSent: Monday"),
            (
                "Approved.",
                Some("________________________________\nFrom: Pat\nSent: Monday")
            )
        );
        // A rule that heads no "From:" is just a rule.
        let rule = "Totals\n__________\nFrom here, it's $40.";
        assert_eq!(
            split_quoted("Totals\n__________\n$40"),
            ("Totals\n__________\n$40", None)
        );
        assert_eq!(split_quoted(rule), (rule, None));
    }

    #[test]
    fn snippets_are_one_capped_line_without_the_quote() {
        assert_eq!(
            snippet(
                "  Sounds\tgood.\n\nSee you\r\nthen.\n\nOn Mon, Oct 5, Sam wrote:\n> Thursday?"
            ),
            "Sounds good. See you then."
        );
        let long = "é".repeat(500);
        assert_eq!(snippet(&long).chars().count(), SNIPPET);
        assert_eq!(snippet(""), "");
    }

    #[test]
    fn inline_and_all_quote_bodies_stay_whole() {
        // Replies between quoted lines need them for context.
        let inline = "> Thursday?\nYes.\n> Who drives?\nMe.";
        assert_eq!(split_quoted(inline), (inline, None));
        // Nothing but quote: hiding it would leave nothing.
        assert_eq!(split_quoted("> fwd\n> fwd"), ("> fwd\n> fwd", None));
        assert_eq!(split_quoted("Plain.\n\n"), ("Plain.\n\n", None));
        assert_eq!(split_quoted(""), ("", None));
    }
}
