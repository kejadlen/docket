//! HTML mail as the thread view frames it (task uylo). Two layers keep
//! it inert: ammonia strips what could act — scripts, forms, event
//! handlers — while keeping the tables, inline styles, and `<style>`
//! blocks mail layout leans on; and the page is served under a CSP
//! that sandboxes it (no script, no same-origin) and blocks remote
//! loads, so a remote image, which doubles as a tracking pixel, loads
//! only when someone asks.

use std::collections::HashSet;
use std::sync::LazyLock;

use ammonia::Builder;

/// Attributes on any tag that carry mail's layout. `style` can't run
/// script in a sandbox, and what it could fetch, the CSP blocks.
const LAYOUT_ATTRIBUTES: &[&str] = &[
    "align",
    "bgcolor",
    "border",
    "cellpadding",
    "cellspacing",
    "class",
    "color",
    "dir",
    "face",
    "height",
    "id",
    "size",
    "style",
    "valign",
    "width",
];

static SANITIZER: LazyLock<Builder<'static>> = LazyLock::new(|| {
    let mut builder = Builder::default();
    // `<style>` is dropped wholesale by default; mail styles itself
    // with it, so it's kept as a tag instead.
    builder
        .rm_clean_content_tags(HashSet::from(["style"]))
        .add_tags(["style", "font", "center"])
        .add_generic_attributes(LAYOUT_ATTRIBUTES)
        .link_rel(Some("noopener noreferrer"))
        // Links leave the frame for a new tab, never navigate it.
        .set_tag_attribute_value("a", "target", "_blank");
    builder
});

/// The mail's HTML with anything that could act stripped out.
pub fn sanitize(raw: &str) -> String {
    SANITIZER.clean(raw).to_string()
}

/// The page the frame loads: the sanitized mail on a plain white ground,
/// as senders design for, with images scaled to fit.
pub fn document(raw: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta name=\"color-scheme\" content=\"light\">\
         <style>html{{background:#fff;color:#1a1a1a}}\
         body{{margin:0;font:15px/1.5 system-ui,sans-serif;overflow-wrap:anywhere}}\
         img{{max-width:100%;height:auto}}</style>\
         </head><body>{}</body></html>",
        sanitize(raw)
    )
}

/// The page's Content-Security-Policy. The sandbox allows only popups,
/// so links open in new tabs that escape it; nothing else runs or
/// loads, except inline images and — once asked for — remote ones.
pub fn csp(remote_images: bool) -> String {
    let images = if remote_images {
        "data: https: http:"
    } else {
        "data:"
    };
    format!(
        "default-src 'none'; style-src 'unsafe-inline'; img-src {images}; font-src data:; \
         form-action 'none'; base-uri 'none'; frame-ancestors 'self'; \
         sandbox allow-popups allow-popups-to-escape-sandbox"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_could_act_is_stripped() {
        let clean = sanitize(
            r#"<p onclick="steal()">Hi<script>steal()</script></p>
               <form action="https://evil.example"><input name="pw"><button>Go</button></form>
               <a href="javascript:steal()">bad</a><iframe src="https://evil.example"></iframe>"#,
        );
        for gone in [
            "onclick",
            "script",
            "steal",
            "form",
            "input",
            "button",
            "javascript",
            "iframe",
        ] {
            assert!(!clean.contains(gone), "{gone} survived: {clean}");
        }
        assert!(clean.contains("<p>Hi</p>"), "{clean}");
    }

    #[test]
    fn layout_survives() {
        let clean = sanitize(
            r##"<style>.cta { color: red; }</style>
               <table width="600" cellpadding="0" bgcolor="#eee"><tr>
               <td class="cta" style="padding: 8px" align="center">
               <font face="Georgia" color="red">Sale</font></td></tr></table>
               <img src="https://shop.example/hero.png" width="600" alt="Hero">"##,
        );
        for kept in [
            "<style>.cta { color: red; }</style>",
            r#"width="600""#,
            r#"cellpadding="0""#,
            r##"bgcolor="#eee""##,
            r#"class="cta""#,
            r#"style="padding: 8px""#,
            r#"align="center""#,
            r#"<font face="Georgia" color="red">"#,
            r#"src="https://shop.example/hero.png""#,
        ] {
            assert!(clean.contains(kept), "{kept} lost: {clean}");
        }
    }

    #[test]
    fn links_open_in_a_new_tab() {
        let clean = sanitize(r#"<a href="https://example.com" target="_self">Track</a>"#);
        assert!(clean.contains(r#"target="_blank""#), "{clean}");
        assert!(clean.contains(r#"rel="noopener noreferrer""#), "{clean}");
        assert!(!clean.contains("_self"), "{clean}");
    }

    #[test]
    fn the_document_wraps_the_sanitized_mail() {
        let page = document("<p>Hi</p><script>x()</script>");
        assert!(page.starts_with("<!doctype html>"));
        assert!(page.contains("<body><p>Hi</p></body>"), "{page}");
    }

    #[test]
    fn remote_images_load_only_when_asked() {
        let blocked = csp(false);
        assert!(blocked.contains("img-src data:;"), "{blocked}");
        assert!(!blocked.contains("https:"), "{blocked}");
        assert!(csp(true).contains("img-src data: https: http:;"));
        for policy in [blocked, csp(true)] {
            assert!(policy.contains("default-src 'none'"));
            assert!(policy.contains("sandbox allow-popups allow-popups-to-escape-sandbox"));
            assert!(!policy.contains("allow-scripts"));
            assert!(!policy.contains("allow-same-origin"));
        }
    }
}
