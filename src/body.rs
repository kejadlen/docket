//! Message bodies as the thread view shows them: plain text, escaped,
//! with its URLs made into links.

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
}
