//! Convert Microsoft Learn markdown to XHTML for EPUB.
//!
//! Learn markdown is CommonMark plus a few house extensions. We preprocess the
//! non-standard bits, run pulldown-cmark, then lightly fix HTML void tags so the output
//! is XHTML-parseable. This is deliberately pragmatic for the vertical slice; the full
//! `:::` zone/pivot handling is a later task (see PLAN section 8).

use pulldown_cmark::{html, Options, Parser};

/// Learn docs base for absolutising root-relative links like `/azure/...`.
const LEARN_DOCS_BASE: &str = "https://learn.microsoft.com";

/// Convert a unit's markdown to an XHTML fragment.
///
/// `media_base` is the absolute raw URL of the module folder (e.g.
/// `https://raw.githubusercontent.com/.../design-governance`) so `../media/x.png`
/// references resolve to something fetchable.
pub fn markdown_to_xhtml(md: &str, media_base: &str) -> String {
    markdown_to_xhtml_with_unit(md, media_base, "")
}

/// Like [`markdown_to_xhtml`], but `unit_url` (the unit's Learn page) becomes the target for
/// video references. Learn's embed-player URLs are JS-only iframes with no inline source, so
/// a direct link to one renders blank in most EPUB readers - linking to the unit page plays
/// the video in context and opens in any browser. Pass `""` to fall back to the raw URL.
pub fn markdown_to_xhtml_with_unit(md: &str, media_base: &str, unit_url: &str) -> String {
    let pre = preprocess(md, media_base, unit_url);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_FOOTNOTES);
    let parser = Parser::new_ext(&pre, opts);
    let mut html_out = String::new();
    html::push_html(&mut html_out, parser);
    strip_code_lang_classes(&xhtmlify(&html_out))
}

/// Remove `class="language-..."` from fenced code blocks so readers do not syntax-highlight
/// them (which adds a reader-specific background and colored keywords). The block keeps the
/// plain background from our stylesheet and renders as uniform monospace text, matching the
/// inline-code treatment. Inline code carries no language class, so it is untouched.
fn strip_code_lang_classes(html: &str) -> String {
    let needle = " class=\"language-";
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(pos) = rest.find(needle) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos + needle.len()..];
        match tail.find('"') {
            Some(q) => rest = &tail[q + 1..],
            None => {
                rest = tail;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Build the markdown for a video reference. Prefer the unit's Learn page (`unit_url`, which
/// actually plays the video); fall back to a normalised player URL when it is unknown.
fn video_link(raw_url: &str, unit_url: &str) -> String {
    let href = if !unit_url.is_empty() {
        unit_url.to_string()
    } else {
        let u = raw_url.trim();
        if u.starts_with("http") {
            u.to_string()
        } else {
            // Bare video id (the prefix-less case) -> the canonical Learn vod player URL.
            format!("https://learn-video.azurefd.net/vod/player?id={u}")
        }
    };
    format!("\n\n[Watch video on Microsoft Learn]({href})\n\n")
}

/// Handle Learn-specific syntax before CommonMark parsing.
fn preprocess(md: &str, media_base: &str, unit_url: &str) -> String {
    let md = strip_iframes(md, unit_url);
    // `:::image ... :::` directives can appear inline (inside HTML table cells, mid-text) as
    // icons, so convert every occurrence to an inline <img>, not just line-leading ones.
    let md = convert_image_directives(&md, media_base);
    let mut out = String::with_capacity(md.len());
    for line in md.lines() {
        let trimmed = line.trim_start();

        // Drop remaining triple-colon zone markers (`:::row:::`, `:::zone ...`, bare `:::`,
        // `:::image-end:::`) so they do not render as literal text; inner content still flows.
        if trimmed.starts_with(":::") {
            continue;
        }

        // > [!VIDEO https://...]  ->  a plain video link paragraph.
        if let Some(url) = trimmed
            .strip_prefix("> [!VIDEO ")
            .and_then(|s| s.strip_suffix(']'))
        {
            out.push_str(&video_link(url, unit_url));
            continue;
        }

        // > [!NOTE] / [!TIP] / [!IMPORTANT] / [!WARNING] / [!CAUTION]  ->  bold label,
        // keeping it a blockquote so the alert still reads as an aside.
        if let Some(rest) = trimmed.strip_prefix("> [!") {
            if let Some(end) = rest.find(']') {
                let kind = &rest[..end];
                let after = rest[end + 1..].trim();
                out.push_str(&format!("> **{}**", title_case(kind)));
                if !after.is_empty() {
                    out.push_str(&format!(" {after}"));
                }
                out.push('\n');
                continue;
            }
        }

        out.push_str(&rewrite_links(line, media_base));
        out.push('\n');
    }
    out
}

/// Replace raw `<iframe ... src="URL" ...></iframe>` embeds (videos) with a plain link.
/// Raw iframes carry boolean attributes (`allowfullscreen`) and unescaped `&` in the URL,
/// neither of which is valid XHTML.
fn strip_iframes(md: &str, unit_url: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(start) = rest.find("<iframe") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let (consumed, src) = if let Some(end) = after.find("</iframe>") {
            (end + "</iframe>".len(), extract_attr(&after[..end], "src"))
        } else if let Some(gt) = after.find('>') {
            (gt + 1, extract_attr(&after[..gt], "src"))
        } else {
            (after.len(), None)
        };
        if let Some(url) = src {
            out.push_str(&video_link(&url, unit_url));
        }
        rest = &after[consumed..];
    }
    out.push_str(rest);
    out
}

/// Replace every Learn `:::image ... :::` directive (block or inline) with an `<img>`,
/// resolving the `source` to an absolute URL. `:::image-end:::` and sourceless directives
/// collapse to nothing.
fn convert_image_directives(md: &str, media_base: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(start) = rest.find(":::image") {
        out.push_str(&rest[..start]);
        let after = &rest[start + ":::image".len()..];
        match after.find(":::") {
            Some(end) => {
                let attrs = &after[..end];
                if let Some(src) = extract_attr(attrs, "source") {
                    let alt = extract_attr(attrs, "alt-text").unwrap_or_default();
                    let url = resolve_media_url(&src, media_base);
                    // Mark icons (sized small) vs content images (capped + centered).
                    let cls = if extract_attr(attrs, "type").as_deref() == Some("icon") {
                        " class=\"learn-icon\""
                    } else {
                        " class=\"content-img\""
                    };
                    out.push_str(&format!(
                        "<img{cls} src=\"{}\" alt=\"{}\"/>",
                        attr_esc(&url),
                        attr_esc(&alt)
                    ));
                }
                rest = &after[end + 3..];
            }
            None => {
                // No closing fence; leave the text as-is and stop.
                out.push_str(":::image");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Resolve a Learn media reference (`../media/x.png`, `media/x.png`, `/azure/...`, absolute)
/// to a fetchable URL.
fn resolve_media_url(src: &str, media_base: &str) -> String {
    if src.starts_with("http") {
        src.to_string()
    } else if let Some(r) = src.strip_prefix("../media/") {
        format!("{media_base}/media/{r}")
    } else if let Some(r) = src.strip_prefix("media/") {
        format!("{media_base}/media/{r}")
    } else if let Some(r) = src.strip_prefix("../") {
        format!("{media_base}/{r}")
    } else if let Some(r) = src.strip_prefix('/') {
        format!("{LEARN_DOCS_BASE}/{r}")
    } else {
        format!("{media_base}/{src}")
    }
}

/// Escape a value for use inside an XML double-quoted attribute.
fn attr_esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Extract `attr="value"` from a tag string.
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{attr}=\"");
    let i = tag.find(&needle)? + needle.len();
    let end = tag[i..].find('"')?;
    Some(tag[i..i + end].to_string())
}

/// Absolutise root-relative `(/...)` links and `../media/...` image paths.
fn rewrite_links(line: &str, media_base: &str) -> String {
    let mut s = line.to_string();
    if s.contains("](/") {
        s = s.replace("](/", &format!("]({LEARN_DOCS_BASE}/"));
    }
    if s.contains("](../media/") {
        s = s.replace("](../media/", &format!("]({media_base}/media/"));
    }
    if s.contains("](media/") {
        s = s.replace("](media/", &format!("]({media_base}/media/"));
    }
    s
}

fn title_case(s: &str) -> String {
    let lower = s.to_lowercase();
    let mut c = lower.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => lower,
    }
}

/// pulldown self-closes the void elements *it* emits, but Learn content embeds raw HTML
/// (tables with `<br>`, `<br >` ..., and `&` in URLs) that passes through verbatim. Normalise
/// void tags and escape stray `&` so the fragment parses as XHTML.
fn xhtmlify(html: &str) -> String {
    let closed = close_void_tag(&close_void_tag(html, "br"), "hr");
    escape_stray_amp(&closed)
}

/// Escape any `&` that does not already begin a valid character reference, without
/// double-escaping existing entities like `&amp;` or `&#160;`.
fn escape_stray_amp(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if c == '&' && !starts_entity(&s[i..]) {
            out.push_str("&amp;");
        } else {
            out.push(c);
        }
    }
    out
}

/// Does `s` (which starts at an `&`) begin a valid entity: `&name;`, `&#123;`, or `&#xAF;`?
fn starts_entity(s: &str) -> bool {
    let rest = &s[1..];
    if let Some(num) = rest.strip_prefix('#') {
        let (digits, is_hex) = match num.strip_prefix(['x', 'X']) {
            Some(h) => (h, true),
            None => (num, false),
        };
        let len = digits
            .chars()
            .take_while(|c| {
                if is_hex {
                    c.is_ascii_hexdigit()
                } else {
                    c.is_ascii_digit()
                }
            })
            .count();
        len > 0 && digits[len..].starts_with(';')
    } else {
        let len = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).count();
        len > 0 && rest[len..].starts_with(';')
    }
}

/// Rewrite `<tag ...>` (in any whitespace/`/` form) to `<tag .../>`. Leaves unrelated
/// tags like `<break>` untouched by requiring a delimiter after the tag name.
fn close_void_tag(html: &str, tag: &str) -> String {
    let needle = format!("<{tag}");
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while let Some(rel) = html[i..].find(&needle) {
        let pos = i + rel;
        out.push_str(&html[i..pos]);
        let after = pos + needle.len();
        // The char after the tag name must delimit it, else it's another tag.
        if !matches!(
            bytes.get(after),
            Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'/') | Some(b'>')
        ) {
            out.push_str(&needle);
            i = after;
            continue;
        }
        match html[after..].find('>') {
            Some(gt) => {
                let inner = html[after..after + gt].trim().trim_end_matches('/').trim_end();
                out.push('<');
                out.push_str(tag);
                if !inner.is_empty() {
                    out.push(' ');
                    out.push_str(inner);
                }
                out.push_str("/>");
                i = after + gt + 1;
            }
            None => {
                out.push_str(&html[pos..]);
                return out;
            }
        }
    }
    out.push_str(&html[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_basic_markdown() {
        let x = markdown_to_xhtml("# Hi\n\nSome **bold** text.", "https://x/mod");
        assert!(x.contains("<h1>Hi</h1>"));
        assert!(x.contains("<strong>bold</strong>"));
    }

    #[test]
    fn absolutises_root_links() {
        let x = markdown_to_xhtml("See [policy](/azure/governance/policy).", "https://x/mod");
        assert!(x.contains("https://learn.microsoft.com/azure/governance/policy"));
    }

    #[test]
    fn video_becomes_link() {
        let x = markdown_to_xhtml("> [!VIDEO https://aka.ms/v]", "https://x/mod");
        assert!(x.contains("https://aka.ms/v"));
        assert!(!x.contains("[!VIDEO"));
    }

    #[test]
    fn video_links_to_unit_page_when_known() {
        // The embed player is JS-only; the unit's Learn page is the working target.
        let x = markdown_to_xhtml_with_unit(
            "> [!VIDEO https://learn-video.azurefd.net/vod/player?id=abc]",
            "https://x/mod",
            "https://learn.microsoft.com/training/modules/m/2-unit",
        );
        assert!(x.contains("https://learn.microsoft.com/training/modules/m/2-unit"));
        assert!(x.contains("Watch video on Microsoft Learn"));
    }

    #[test]
    fn bare_video_id_gets_player_prefix() {
        // No unit_url and a prefix-less id -> normalise to the canonical player URL.
        let x = markdown_to_xhtml("> [!VIDEO 477e6b92-9bc6-425d-90fe-2468ab8ab0f1]", "https://x/mod");
        assert!(x.contains("https://learn-video.azurefd.net/vod/player?id=477e6b92-9bc6-425d-90fe-2468ab8ab0f1"));
    }

    #[test]
    fn note_alert_labelled() {
        let x = markdown_to_xhtml("> [!NOTE]\n> Be careful.", "https://x/mod");
        assert!(x.contains("Note"));
        assert!(!x.contains("[!NOTE]"));
    }

    #[test]
    fn iframe_becomes_link_and_amp_escaped() {
        let md =
            "<iframe width=\"854\" src=\"https://youtube.com/embed/x?a=1&b=2\" allowfullscreen></iframe>";
        // With no unit_url, the iframe src is kept as the fallback target.
        let x = markdown_to_xhtml(md, "https://x/mod");
        assert!(!x.contains("<iframe"));
        assert!(!x.contains("allowfullscreen"));
        assert!(x.contains("Watch video on Microsoft Learn"));
        // the & in the URL is escaped, and not double-escaped
        assert!(x.contains("a=1&amp;b=2"));
        assert!(!x.contains("&amp;amp;"));
    }

    #[test]
    fn image_zone_converted() {
        let x = markdown_to_xhtml(
            ":::image type=\"content\" source=\"../media/x.png\" alt-text=\"A diagram\":::",
            "https://raw/mod",
        );
        assert!(x.contains("<img"));
        assert!(x.contains("https://raw/mod/media/x.png"));
        assert!(x.contains("A diagram"));
    }

    #[test]
    fn inline_image_directive_in_html_table() {
        let md = "<table><tr><td>:::image type=\"icon\" source=\"../media/i.png\":::</td></tr></table>";
        let x = markdown_to_xhtml(md, "https://raw/mod");
        assert!(!x.contains(":::image"), "directive leaked: {x}");
        assert!(x.contains("src=\"https://raw/mod/media/i.png\""));
        assert!(x.contains("class=\"learn-icon\""), "icon class missing: {x}");
    }

    #[test]
    fn image_end_and_bare_fences_dropped() {
        let x = markdown_to_xhtml(":::row:::\ntext\n:::image-end:::", "https://raw/mod");
        assert!(!x.contains(":::"), "fence leaked: {x}");
        assert!(x.contains("text"));
    }

    #[test]
    fn closes_raw_void_tags() {
        // Raw HTML passed through from a Learn table, with messy <br> variants.
        let md = "<p>a <br> b <br > c <br/> d <br /> e</p>";
        let x = markdown_to_xhtml(md, "https://x/mod");
        assert!(!x.contains("<br>"));
        assert!(!x.contains("<br >"));
        assert_eq!(x.matches("<br/>").count(), 4);
        // does not maul a non-void tag that merely starts with the same letters
        let x2 = markdown_to_xhtml("<p><break>x</break></p>", "https://x/mod");
        assert!(x2.contains("<break>"));
    }
}
