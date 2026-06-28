//! Fallback content source: scrape a Microsoft Learn **unit page** for modules whose Markdown
//! is not published in the public `MicrosoftDocs/learn` repo (e.g. the Microsoft 365 admin
//! track). Learn unit pages are server-rendered, so a plain HTTP GET returns the full prose in
//! static HTML - no headless browser. We isolate the stable `#module-unit-content` container
//! and re-serialize it into the same clean XHTML subset the Markdown path emits, so scraped
//! units drop straight onto the existing Book / EPUB structure.
//!
//! Images are rewritten to absolute URLs so the normal `embed_images` pass can base64-embed
//! them (it only touches `src="http..."`). Page chrome (header, nav, metadata, feedback) lives
//! outside `#module-unit-content`, so isolating that node drops it for free.

use scraper::{Html, Node, Selector};

/// Extract a unit page's prose as engine XHTML. `page_url` is the absolute unit URL, used to
/// resolve relative image references. Returns `None` if the content container is absent (a 404
/// page or an unexpected layout), so the caller can fall back to a placeholder.
pub fn extract_unit_xhtml(page_html: &str, page_url: &str) -> Option<String> {
    let doc = Html::parse_document(page_html);
    // `#module-unit-content` is the rendered-Markdown container; the unit title and all
    // surrounding chrome sit outside it.
    let sel = Selector::parse("#module-unit-content").ok()?;
    let content = doc.select(&sel).next()?;
    let mut out = String::new();
    for child in content.children() {
        serialize_node(child, page_url, &mut out);
    }
    let trimmed = out.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

// Block/inline tags passed through unchanged (the XHTML subset the EPUB accepts).
const PASSTHROUGH: &[&str] = &[
    "h2", "h3", "h4", "h5", "h6", "p", "ul", "ol", "li", "strong", "em", "b", "i", "code", "pre",
    "blockquote", "table", "thead", "tbody", "tr", "th", "td", "sup", "sub", "dl", "dt", "dd",
];
// Void elements: self-closing, no children.
const VOID: &[&str] = &["br", "hr"];
// Elements dropped entirely (interactive / non-content), children and all.
const DROP: &[&str] = &[
    "script", "style", "button", "nav", "svg", "form", "input", "iframe", "video", "audio",
    "noscript", "select", "option", "template", "head",
];
// Substrings that mark a Learn alert/callout container -> rendered as a blockquote.
const ALERT_HINTS: &[&str] = &["alert", "note", "tip", "important", "warning", "caution"];

fn serialize_node(node: ego_tree::NodeRef<'_, Node>, page_url: &str, out: &mut String) {
    match node.value() {
        Node::Text(t) => push_escaped_text(&t.text, out),
        Node::Element(el) => {
            let name = el.name();
            if DROP.contains(&name) {
                return;
            }
            if name == "img" {
                if let Some(src) = el.attr("src") {
                    let abs = resolve_url(page_url, src);
                    out.push_str(&format!(
                        "<img src=\"{}\" alt=\"{}\"/>",
                        esc_attr(&abs),
                        esc_attr(el.attr("alt").unwrap_or("")),
                    ));
                }
                return;
            }
            if VOID.contains(&name) {
                out.push_str(&format!("<{name}/>"));
                return;
            }
            if name == "a" {
                // Keep links with an absolute-resolvable href; otherwise just unwrap the text.
                if let Some(href) = el.attr("href").filter(|h| !h.starts_with('#')) {
                    let abs = resolve_url(page_url, href);
                    out.push_str(&format!("<a href=\"{}\">", esc_attr(&abs)));
                    recurse(node, page_url, out);
                    out.push_str("</a>");
                } else {
                    recurse(node, page_url, out);
                }
                return;
            }
            // Learn callouts (note/tip/warning) come as <div class="alert ...">: render as a
            // blockquote so the box reads as an aside rather than vanishing.
            if is_alert(el.attr("class")) {
                out.push_str("<blockquote>");
                recurse(node, page_url, out);
                out.push_str("</blockquote>");
                return;
            }
            let tag = map_tag(name);
            match tag {
                Some(t) => {
                    out.push_str(&format!("<{t}>"));
                    recurse(node, page_url, out);
                    out.push_str(&format!("</{t}>"));
                }
                // Unknown / structural wrapper: unwrap (emit children only).
                None => recurse(node, page_url, out),
            }
        }
        _ => {}
    }
}

fn recurse(node: ego_tree::NodeRef<'_, Node>, page_url: &str, out: &mut String) {
    for child in node.children() {
        serialize_node(child, page_url, out);
    }
}

/// Map a source tag to the emitted XHTML tag. `h1` becomes `h2` (the chapter already carries the
/// unit title as its heading). Passthrough tags map to themselves; everything else returns
/// `None` and is unwrapped.
fn map_tag(name: &str) -> Option<&'static str> {
    match name {
        "h1" => Some("h2"),
        _ => PASSTHROUGH.iter().copied().find(|&t| t == name),
    }
}

fn is_alert(class: Option<&str>) -> bool {
    class
        .map(|c| {
            let c = c.to_lowercase();
            ALERT_HINTS.iter().any(|h| c.contains(h))
        })
        .unwrap_or(false)
}

fn push_escaped_text(s: &str, out: &mut String) {
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
}

fn esc_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Resolve a possibly-relative URL against the absolute `base` page URL. Handles absolute,
/// scheme-relative (`//`), root-relative (`/`), and dotted relative (`../`, `./`) forms.
pub fn resolve_url(base: &str, rel: &str) -> String {
    if rel.starts_with("http://") || rel.starts_with("https://") {
        return rel.to_string();
    }
    if let Some(stripped) = rel.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    let (scheme_host, base_path) = split_origin(base);
    if let Some(abs_path) = rel.strip_prefix('/') {
        return format!("{scheme_host}/{abs_path}");
    }
    // Relative: drop the base's last segment (the "file"), then fold ./ and ../.
    let mut segs: Vec<&str> = base_path.split('/').filter(|s| !s.is_empty()).collect();
    segs.pop(); // remove the unit "file" segment
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            other => segs.push(other.split(['?', '#']).next().unwrap_or(other)),
        }
    }
    format!("{scheme_host}/{}", segs.join("/"))
}

/// Split `https://host/a/b/c?x` into (`https://host`, `/a/b/c`).
fn split_origin(url: &str) -> (String, String) {
    let no_scheme = url.split("://").nth(1).unwrap_or(url);
    let scheme = url.split("://").next().unwrap_or("https");
    let path_start = no_scheme.find('/').unwrap_or(no_scheme.len());
    let host = &no_scheme[..path_start];
    let path = &no_scheme[path_start..];
    let path = path.split(['?', '#']).next().unwrap_or(path);
    (format!("{scheme}://{host}"), path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_image_urls() {
        let base = "https://learn.microsoft.com/en-us/training/modules/configure-microsoft-365-experience/2-explore-your-microsoft-365-cloud-environment";
        assert_eq!(
            resolve_url(base, "../../wwl/configure-microsoft-365-experience/media/x.png"),
            "https://learn.microsoft.com/en-us/training/wwl/configure-microsoft-365-experience/media/x.png"
        );
        assert_eq!(
            resolve_url(base, "/en-us/media/y.png"),
            "https://learn.microsoft.com/en-us/media/y.png"
        );
        assert_eq!(resolve_url(base, "https://cdn/z.png"), "https://cdn/z.png");
        assert_eq!(resolve_url(base, "//cdn.example/z.png"), "https://cdn.example/z.png");
    }

    #[test]
    fn extracts_content_and_drops_chrome() {
        let html = r#"<html><body>
            <header id="article-header">Read in English Add to plan</header>
            <main>
              <h1 id="module-unit-title">The Title</h1>
              <div id="module-unit-content">
                <h2>Section</h2>
                <p>Hello <strong>world</strong> &amp; friends.</p>
                <div class="alert alert-info"><p>Heads up.</p></div>
                <img src="media/pic.png" alt="A pic"/>
                <button>Click</button>
                <script>evil()</script>
              </div>
              <div id="ms--unit-user-feedback">Was this helpful?</div>
            </main></body></html>"#;
        let x = extract_unit_xhtml(html, "https://learn.microsoft.com/en-us/training/modules/m/1-intro").unwrap();
        assert!(x.contains("<h2>Section</h2>"));
        assert!(x.contains("<p>Hello <strong>world</strong> &amp; friends.</p>"));
        assert!(x.contains("<blockquote><p>Heads up.</p></blockquote>"));
        assert!(x.contains(r#"<img src="https://learn.microsoft.com/en-us/training/modules/m/media/pic.png" alt="A pic"/>"#));
        // chrome + interactive dropped
        assert!(!x.contains("Read in English"));
        assert!(!x.contains("Was this helpful"));
        assert!(!x.contains("Click"));
        assert!(!x.contains("evil"));
    }
}
