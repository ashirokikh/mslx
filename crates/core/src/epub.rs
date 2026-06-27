//! Minimal EPUB 3 packager: takes a set of XHTML chapters and zips a valid EPUB.
//!
//! An EPUB is a ZIP with a fixed layout: an uncompressed `mimetype` first, a
//! `META-INF/container.xml` pointing at the package document, then the OPF (metadata +
//! manifest + spine), a nav document (the TOC), and the XHTML chapters. We hand-build the
//! XML so there are no heavy deps and the same code can target wasm later.

use std::io::Write;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

/// One chapter in reading order. `body` is an XHTML fragment (already converted).
pub struct Chapter {
    pub id: String,
    pub filename: String,
    pub title: String,
    pub body: String,
    /// When set (the first unit of a module), the module name is shown as the section
    /// heading above the unit title, so the reader sees which module they are entering.
    pub module_header: Option<String>,
}

/// A node in the navigation document (the TOC). Children allow a nested TOC
/// (part -> module -> unit) independent of the linear spine.
pub struct NavEntry {
    pub href: String,
    pub title: String,
    pub children: Vec<NavEntry>,
}

impl NavEntry {
    pub fn leaf(href: impl Into<String>, title: impl Into<String>) -> Self {
        NavEntry {
            href: href.into(),
            title: title.into(),
            children: Vec::new(),
        }
    }
}

/// A binary asset (image) embedded into the EPUB. `filename` is relative to OEBPS
/// (e.g. `media/img001.png`).
pub struct Resource {
    pub filename: String,
    pub media_type: String,
    pub data: Vec<u8>,
}

const CSS: &str = r#"
body { font-family: sans-serif; line-height: 1.5; margin: 1em; }
h1, h2, h3 { font-family: sans-serif; line-height: 1.2; }
.module-name { border-top: 2px solid #b9c6cf; padding-top: 0.4em; color: #0b3d5c; }
.module-name + h2 { margin-top: 0.2em; color: #345; font-weight: normal; }
code, pre { font-family: monospace; }
pre { white-space: pre-wrap; background: #f4f4f4; padding: 0.6em; }
blockquote { border-left: 3px solid #bbb; margin: 1em 0; padding: 0.2em 1em; color: #333; }
img { max-width: 100%; height: auto; }
img.content-img { display: block; max-width: 100%; height: auto; margin: 0.6em auto; }
.quiz-q { margin: 1.2em 0; }
.quiz-choices { list-style-type: upper-alpha; margin: 0.3em 0 0.5em 1.4em; }
.quiz-choices li { margin: 0.2em 0; }
.answer-key { font-family: sans-serif; color: #0b3d5c; margin-top: 1.2em; }
.answer { color: #333; font-size: 0.95em; margin: 0.3em 0; }
.muted { color: #666; font-size: 0.9em; }
.badge-wrap { text-align: center; margin: 0.5em 0 1em; }
img.badge { max-width: 150px; height: auto; }
img.learn-icon { height: 1.1em; width: auto; vertical-align: middle; }
"#;

/// Build a complete EPUB from ordered chapters.
///
/// `title`/`identifier`/`modified` populate the OPF metadata; `modified` must be an
/// `xsd:dateTime` (e.g. `2026-06-15T00:00:00Z`) per EPUB 3.
pub fn build_epub(
    title: &str,
    identifier: &str,
    modified: &str,
    language: &str,
    chapters: &[Chapter],
    nav: &[NavEntry],
    resources: &[Resource],
) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let mut zip = ZipWriter::new(std::io::Cursor::new(&mut buf));
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let deflated =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        // 1. mimetype - must be first and stored uncompressed.
        zip.start_file("mimetype", stored)?;
        zip.write_all(b"application/epub+zip")?;

        // 2. container.xml
        zip.start_file("META-INF/container.xml", deflated)?;
        zip.write_all(CONTAINER_XML.as_bytes())?;

        // 3. stylesheet
        zip.start_file("OEBPS/style.css", deflated)?;
        zip.write_all(CSS.as_bytes())?;

        // 4. nav document (EPUB 3 TOC) + NCX (EPUB 2 TOC, for reader compatibility)
        zip.start_file("OEBPS/nav.xhtml", deflated)?;
        zip.write_all(nav_xhtml(title, nav).as_bytes())?;
        zip.start_file("OEBPS/toc.ncx", deflated)?;
        zip.write_all(ncx(title, identifier, nav).as_bytes())?;

        // 5. chapters
        for ch in chapters {
            zip.start_file(format!("OEBPS/{}", ch.filename), deflated)?;
            zip.write_all(
                chapter_xhtml(&ch.title, ch.module_header.as_deref(), &ch.body).as_bytes(),
            )?;
        }

        // 6. embedded resources (images)
        for r in resources {
            zip.start_file(format!("OEBPS/{}", r.filename), deflated)?;
            zip.write_all(&r.data)?;
        }

        // 7. package document
        zip.start_file("OEBPS/content.opf", deflated)?;
        zip.write_all(opf(title, identifier, modified, language, chapters, resources).as_bytes())?;

        zip.finish()?;
    }
    Ok(buf)
}

const CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#;

fn chapter_xhtml(title: &str, module_header: Option<&str>, body: &str) -> String {
    let heading = match module_header {
        Some(m) => format!(
            "<h1 class=\"module-name\">{}</h1>\n<h2>{}</h2>",
            esc(m),
            esc(title)
        ),
        None => format!("<h1>{}</h1>", esc(title)),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="en">
<head>
  <meta charset="utf-8"/>
  <title>{title}</title>
  <link rel="stylesheet" type="text/css" href="style.css"/>
</head>
<body>
{heading}
{body}
</body>
</html>
"#,
        title = esc(title),
        heading = heading,
        body = body
    )
}

fn nav_xhtml(title: &str, nav: &[NavEntry]) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops" lang="en">
<head>
  <meta charset="utf-8"/>
  <title>{title} - Contents</title>
  <link rel="stylesheet" type="text/css" href="style.css"/>
</head>
<body>
  <nav epub:type="toc" id="toc">
    <h1>Contents</h1>
{list}  </nav>
</body>
</html>
"#,
        title = esc(title),
        list = render_nav_list(nav, 2)
    )
}

fn ncx(title: &str, identifier: &str, nav: &[NavEntry]) -> String {
    let mut order = 0u32;
    let nav_points = render_navpoints(nav, &mut order, 2);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content="{id}"/>
    <meta name="dtb:depth" content="3"/>
    <meta name="dtb:totalPageCount" content="0"/>
    <meta name="dtb:maxPageNumber" content="0"/>
  </head>
  <docTitle><text>{title}</text></docTitle>
  <navMap>
{points}  </navMap>
</ncx>
"#,
        id = esc(identifier),
        title = esc(title),
        points = nav_points
    )
}

fn render_navpoints(entries: &[NavEntry], order: &mut u32, depth: usize) -> String {
    let pad = "  ".repeat(depth);
    let mut s = String::new();
    for e in entries {
        *order += 1;
        let np = *order;
        s.push_str(&format!(
            "{pad}<navPoint id=\"np{np}\" playOrder=\"{np}\">\n{pad}  <navLabel><text>{t}</text></navLabel>\n{pad}  <content src=\"{href}\"/>\n",
            t = esc(&e.title),
            href = esc(&e.href),
        ));
        if !e.children.is_empty() {
            s.push_str(&render_navpoints(&e.children, order, depth + 1));
        }
        s.push_str(&format!("{pad}</navPoint>\n"));
    }
    s
}

fn render_nav_list(entries: &[NavEntry], depth: usize) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let pad = "  ".repeat(depth);
    let mut s = format!("{pad}<ol>\n");
    for e in entries {
        s.push_str(&format!(
            "{pad}  <li><a href=\"{}\">{}</a>",
            esc(&e.href),
            esc(&e.title)
        ));
        if !e.children.is_empty() {
            s.push('\n');
            s.push_str(&render_nav_list(&e.children, depth + 2));
            s.push_str(&format!("{pad}  </li>\n"));
        } else {
            s.push_str("</li>\n");
        }
    }
    s.push_str(&format!("{pad}</ol>\n"));
    s
}

fn opf(
    title: &str,
    identifier: &str,
    modified: &str,
    language: &str,
    chapters: &[Chapter],
    resources: &[Resource],
) -> String {
    let mut manifest = String::new();
    let mut spine = String::new();
    for ch in chapters {
        manifest.push_str(&format!(
            "    <item id=\"{id}\" href=\"{file}\" media-type=\"application/xhtml+xml\"/>\n",
            id = esc(&ch.id),
            file = esc(&ch.filename)
        ));
        spine.push_str(&format!("    <itemref idref=\"{}\"/>\n", esc(&ch.id)));
    }
    for (i, r) in resources.iter().enumerate() {
        manifest.push_str(&format!(
            "    <item id=\"res{i}\" href=\"{file}\" media-type=\"{mt}\"/>\n",
            file = esc(&r.filename),
            mt = esc(&r.media_type)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="bookid">{identifier}</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:language>{language}</dc:language>
    <meta property="dcterms:modified">{modified}</meta>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="css" href="style.css" media-type="text/css"/>
{manifest}  </manifest>
  <spine toc="ncx">
{spine}  </spine>
</package>
"#,
        identifier = esc(identifier),
        title = esc(title),
        language = esc(language),
        modified = esc(modified),
        manifest = manifest,
        spine = spine
    )
}

/// Minimal XML text escaping.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_nonempty_zip_with_mimetype_first() {
        let chapters = vec![Chapter {
            id: "c1".into(),
            filename: "c1.xhtml".into(),
            title: "One".into(),
            body: "<p>hello</p>".into(),
            module_header: None,
        }];
        let nav = vec![NavEntry::leaf("c1.xhtml", "One")];
        let bytes =
            build_epub("T", "urn:x", "2026-06-15T00:00:00Z", "en", &chapters, &nav, &[]).unwrap();
        assert!(bytes.len() > 100);
        // ZIP local file header for the first entry names "mimetype".
        assert_eq!(&bytes[30..38], b"mimetype");
    }
}
