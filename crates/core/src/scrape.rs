//! Fallback content source for modules whose Markdown isn't in the public `MicrosoftDocs/learn`
//! repo (the Microsoft 365 admin track, retired exams, Dynamics, etc.).
//!
//! Microsoft Learn serves each unit's ORIGINAL authored Markdown at
//! `<unit-url>?accept=text/markdown`. We fetch that and run it through the very same
//! [`markdown_to_xhtml_with_unit`] path the GitHub units use, so a scraped unit is formatted
//! identically to a public one - no separate HTML parser, and code fences, lists, tables, links
//! and images all match. Two Learn-specific quirks are normalised here:
//!
//! 1. A YAML frontmatter block and a `Completed` / `- N minutes` metadata preamble are injected
//!    after the title; GitHub units have neither, so we strip them for parity.
//! 2. Images use a unit-relative path (`![](../../<group>/<module>/media/x.png)`). `../../` from
//!    any `/training/modules/<m>/<unit>` page resolves to the `/training/` root, so we rewrite
//!    that prefix to an absolute URL and let the normal image-embed pass pick it up.

use crate::markdown::markdown_to_xhtml_with_unit;

/// Convert a unit's raw Learn Markdown (from `?accept=text/markdown`) into engine XHTML, matching
/// the GitHub Markdown path. `unit_url` is the absolute unit page URL. `None` if the body is empty
/// (e.g. a 404 page served as Markdown).
pub fn unit_markdown_to_xhtml(raw_md: &str, unit_url: &str) -> Option<String> {
    let body = rewrap_alerts(&strip_unit_preamble(strip_frontmatter(raw_md)));
    if body.trim().is_empty() {
        return None;
    }
    let abs = absolutize_media(&body, unit_url);
    Some(markdown_to_xhtml_with_unit(&abs, unit_url, unit_url))
}

/// Learn's Markdown export flattens `> [!IMPORTANT]` alerts to a bare keyword line followed by the
/// body, dropping the callout. Re-wrap a standalone alert keyword (`Note`/`Tip`/`Important`/
/// `Warning`/`Caution`, blank-delimited) plus its following paragraph back into the GitHub
/// `> [!KEYWORD]` blockquote form, so `markdown_to_xhtml` renders it as the same bold-labelled
/// aside a public module gets.
fn rewrap_alerts(md: &str) -> String {
    const KW: &[&str] = &["Note", "Tip", "Important", "Warning", "Caution"];
    let lines: Vec<&str> = md.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let kw = lines[i].trim();
        let standalone = KW.contains(&kw)
            && (i == 0 || lines[i - 1].trim().is_empty())
            && lines.get(i + 1).map(|l| l.trim().is_empty()).unwrap_or(false);
        if standalone {
            // The alert body is the paragraph after the blank, up to the next blank line.
            let mut j = i + 2;
            let mut body: Vec<&str> = Vec::new();
            while j < lines.len() && !lines[j].trim().is_empty() {
                body.push(lines[j]);
                j += 1;
            }
            if !body.is_empty() {
                out.push(format!("> [!{}]", kw.to_uppercase()));
                for b in body {
                    out.push(format!("> {b}"));
                }
                i = j;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    out.join("\n")
}

/// Strip a leading YAML frontmatter block (`---` ... `---`) if present.
pub fn strip_frontmatter(md: &str) -> &str {
    let t = md.trim_start_matches(['\u{feff}', '\r', '\n', ' ']);
    if let Some(rest) = t.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            return rest[end + "\n---".len()..].trim_start_matches(['\r', '\n']);
        }
    }
    md
}

/// Drop the preamble Learn injects at the top of each unit's exported Markdown that GitHub units
/// don't have: the leading `# <title>` heading (the chapter already renders the unit title as its
/// own heading, so keeping it would duplicate it) and the `Completed` / `- N minutes` metadata.
/// Only the first few lines are touched, so real content headings are safe.
fn strip_unit_preamble(body: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut dropped_title = false;
    for (i, line) in body.lines().enumerate() {
        let t = line.trim();
        if !dropped_title && i < 3 && t.starts_with("# ") {
            dropped_title = true;
            continue;
        }
        if i < 8 && (t == "Completed" || is_minutes_line(t)) {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim_start().to_string()
}

fn is_minutes_line(t: &str) -> bool {
    t.strip_prefix("- ")
        .and_then(|r| r.strip_suffix(" minutes").or_else(|| r.strip_suffix(" minute")))
        .map(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

/// Rewrite Learn's unit-relative `](../../...)` image/link targets to absolute URLs. `../../` from
/// a `/training/modules/<m>/<unit>` page is the `/training/` root.
pub fn absolutize_media(md: &str, unit_url: &str) -> String {
    const MARK: &str = "/training/";
    match unit_url.find(MARK) {
        Some(i) => {
            let training_base = &unit_url[..i + MARK.len()];
            md.replace("](../../", &format!("]({training_base}"))
        }
        None => md.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_frontmatter_and_preamble() {
        let md = "---\nuid: x\ntitle: T\n---\n# Title\n\nCompleted\n\n- 10 minutes\n\nReal content here.\n";
        let body = strip_unit_preamble(strip_frontmatter(md));
        // Title h1 dropped (the chapter renders the title itself), metadata dropped, content kept.
        assert!(body.starts_with("Real content here."));
        assert!(!body.contains("# Title"));
        assert!(!body.contains("Completed"));
        assert!(!body.contains("10 minutes"));
        assert!(!body.contains("uid: x"));
    }

    #[test]
    fn absolutizes_unit_relative_images() {
        let url = "https://learn.microsoft.com/en-us/training/modules/configure-microsoft-365-experience/2-explore";
        let md = "![alt](../../wwl/configure-microsoft-365-experience/media/x.png)";
        assert_eq!(
            absolutize_media(md, url),
            "![alt](https://learn.microsoft.com/en-us/training/wwl/configure-microsoft-365-experience/media/x.png)"
        );
    }

    #[test]
    fn converts_unit_markdown_to_xhtml() {
        let url = "https://learn.microsoft.com/en-us/training/modules/m/2-explore";
        let md = "---\nuid: x\n---\n# Unit Title\n\nCompleted\n\n- 5 minutes\n\nA paragraph with `code`.\n\n## Section\n\n```json\n{}\n```\n";
        let x = unit_markdown_to_xhtml(md, url).unwrap();
        // Leading title h1 is dropped (chapter renders it); a content h2 survives; inline code
        // gets the `ic` class and fenced code keeps `language-*` - identical to the GitHub path.
        assert!(!x.contains("<h1>Unit Title</h1>"));
        assert!(x.contains("<h2>Section</h2>"));
        assert!(x.contains("<p>A paragraph with <code class=\"ic\">code</code>.</p>"));
        assert!(x.contains("<pre><code class=\"language-json\">"));
        assert!(!x.contains("Completed"));
    }

    #[test]
    fn rewraps_flattened_alerts_into_callouts() {
        // Learn flattens "> [!IMPORTANT]" to a bare keyword + paragraph; we restore the callout.
        let url = "https://learn.microsoft.com/en-us/training/modules/m/1-x";
        let md = "---\nuid: x\n---\nIntro line.\n\nImportant\n\nAzure AD is now Microsoft Entra ID.\n\nNext paragraph.\n";
        let x = unit_markdown_to_xhtml(md, url).unwrap();
        // Renders as a blockquote with a bold label (same as the GitHub `> [!IMPORTANT]` path).
        assert!(x.contains("<blockquote>"));
        assert!(x.contains("<strong>Important</strong>"));
        assert!(x.contains("Azure AD is now Microsoft Entra ID."));
        // The bare keyword line must not survive as plain text.
        assert!(!x.contains("<p>Important</p>"));
    }

    #[test]
    fn empty_body_returns_none() {
        assert!(unit_markdown_to_xhtml("---\nuid: x\n---\n", "https://learn.microsoft.com/training/modules/m/1-x").is_none());
    }
}
