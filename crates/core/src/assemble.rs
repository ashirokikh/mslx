//! High-level assembly: one Learn module -> a complete EPUB.
//!
//! Vertical slice for the project. Resolves a module via the Catalog API, fetches each
//! unit's markdown (and the knowledge-check quiz) from GitHub raw, converts to XHTML, and
//! packages an EPUB with a title page (date stamp), a nav TOC, and a sources appendix.

use crate::epub::{build_epub, esc, Chapter, NavEntry, Resource};
use crate::markdown::markdown_to_xhtml_with_unit;
use crate::quiz::{self, Question, Quiz};
use crate::{
    module_slug_from_url, resolve_certification, resolve_module, unit_slug_from_uid, Book,
    ContentIndex, Fetcher, ResolveError,
};
use futures::stream::{self, StreamExt};
use std::collections::HashMap;

/// Max in-flight fetches. The work is network-latency-bound, so issuing several requests
/// concurrently (rather than one-at-a-time) is the real speedup; capped to stay polite to
/// GitHub. Works the same on the native runtime and the single-threaded wasm one.
const FETCH_CONCURRENCY: usize = 12;

/// Gentler concurrency used whenever a build scrapes Learn pages. learn.microsoft.com rate-limits
/// a heavy burst (unlike GitHub-raw), so we trade some speed to keep scraped units from failing.
const SCRAPE_CONCURRENCY: usize = 4;

/// The mslx project repo, linked from each book's provenance line.
const MSLX_REPO: &str = "github.com/ashirokikh/mslx";

/// Download every `<img>` the chapters reference and embed each as a raster file, rewriting
/// `src` to the local path. SVGs are rasterized to PNG (resvg) so they render in every EPUB
/// reader - inline/`<img>` SVG is unreliable across readers. Failed fetches keep their URL.
async fn embed_images<F: Fetcher + Sync>(fetcher: &F, chapters: &mut [Chapter]) -> Vec<Resource> {
    // Collect unique remote image URLs in first-seen order.
    let mut seen = std::collections::HashSet::new();
    let mut order: Vec<String> = Vec::new();
    for ch in chapters.iter() {
        for url in extract_img_srcs(&ch.body) {
            if url.starts_with("http") && seen.insert(url.clone()) {
                order.push(url);
            }
        }
    }

    // Fetch concurrently: SVG -> fetch text + rasterize to PNG; raster -> fetch bytes.
    let mut fetched: Vec<(usize, String, Option<(String, Vec<u8>)>)> =
        stream::iter(order.into_iter().enumerate())
            .map(|(i, url)| async move {
                let asset = if is_svg_url(&url) {
                    match get_text_retrying(fetcher, &url).await {
                        Ok(svg) => rasterize_svg(&svg).map(|png| ("png".to_string(), png)),
                        Err(_) => None,
                    }
                } else {
                    get_bytes_retrying(fetcher, &url)
                        .await
                        .ok()
                        .map(|b| (ext_from_url(&url), b))
                };
                (i, url, asset)
            })
            .buffer_unordered(FETCH_CONCURRENCY)
            .collect()
            .await;
    fetched.sort_by_key(|(i, _, _)| *i);

    let mut resources = Vec::new();
    let mut local: HashMap<String, String> = HashMap::new();
    let mut n = 0;
    for (_, url, asset) in fetched {
        if let Some((ext, data)) = asset {
            n += 1;
            let filename = format!("media/img{n:04}.{ext}");
            let media_type = media_type_for(&ext);
            local.insert(url, filename.clone());
            resources.push(Resource {
                filename,
                media_type,
                data,
            });
        }
    }

    for ch in chapters.iter_mut() {
        for (url, file) in &local {
            ch.body = ch
                .body
                .replace(&format!("src=\"{url}\""), &format!("src=\"{file}\""));
        }
    }
    resources
}

fn is_svg_url(url: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_lowercase()
        .ends_with(".svg")
}

/// Render an SVG to PNG bytes (~2x for crispness, width capped). Returns None if the SVG
/// cannot be parsed or sized.
fn rasterize_svg(svg: &str) -> Option<Vec<u8>> {
    use resvg::{tiny_skia, usvg};
    #[allow(unused_mut)]
    let mut opt = usvg::Options::default();
    // Without a font database, resvg silently drops every <text> element - MS Learn
    // diagrams (Calibri labels) rasterize to shapes with no words. On native we load the
    // system fonts and point the generic families at fonts that exist here so the
    // `...,sans-serif` fallback in those SVGs resolves. On wasm there are no system fonts
    // to load, so we embed a Latin subset of Carlito (the metric-compatible Calibri clone)
    // and resolve every generic family to it - the same diagrams render with text in the
    // browser too.
    #[cfg(target_arch = "wasm32")]
    {
        let db = opt.fontdb_mut();
        db.load_font_data(include_bytes!("../fonts/Carlito-Regular.subset.ttf").to_vec());
        db.load_font_data(include_bytes!("../fonts/Carlito-Bold.subset.ttf").to_vec());
        db.set_sans_serif_family("Carlito");
        db.set_serif_family("Carlito");
        db.set_monospace_family("Carlito");
        opt.font_family = "Carlito".to_string();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let db = opt.fontdb_mut();
        db.load_system_fonts();
        let have: std::collections::HashSet<&str> = db
            .faces()
            .flat_map(|f| f.families.iter().map(|(n, _)| n.as_str()))
            .collect();
        let pick = |cands: &[&'static str], default: &'static str| -> &'static str {
            cands.iter().copied().find(|n| have.contains(n)).unwrap_or(default)
        };
        let sans = pick(
            &["Carlito", "Liberation Sans", "DejaVu Sans", "Noto Sans", "Arial"],
            "DejaVu Sans",
        );
        let serif = pick(
            &["Liberation Serif", "DejaVu Serif", "Noto Serif", "Times New Roman"],
            "DejaVu Serif",
        );
        let mono = pick(
            &["Liberation Mono", "DejaVu Sans Mono", "Noto Sans Mono", "Courier New"],
            "DejaVu Sans Mono",
        );
        db.set_sans_serif_family(sans);
        db.set_serif_family(serif);
        db.set_monospace_family(mono);
        opt.font_family = sans.to_string();
    }
    let tree = usvg::Tree::from_str(svg, &opt).ok()?;
    let size = tree.size();
    let (w, h) = (size.width(), size.height());
    if !(w > 0.0 && h > 0.0) {
        return None;
    }
    let scale = (2.0_f32).min(1500.0 / w).max(0.05);
    let pw = (w * scale).ceil().max(1.0) as u32;
    let ph = (h * scale).ceil().max(1.0) as u32;
    let mut pixmap = tiny_skia::Pixmap::new(pw, ph)?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap.encode_png().ok()
}

fn extract_img_srcs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(i) = rest.find("<img") {
        let after = &rest[i + 4..];
        if let Some(s) = after.find("src=\"") {
            let start = s + 5;
            if let Some(e) = after[start..].find('"') {
                out.push(after[start..start + e].to_string());
                rest = &after[start + e..];
                continue;
            }
        }
        rest = after;
    }
    out
}

fn ext_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('.')
        .next()
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("png")
        .to_lowercase()
}

fn media_type_for(ext: &str) -> String {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        _ => "image/png",
    }
    .to_string()
}

struct SourceRef {
    title: String,
    learn_url: String,
    raw_url: String,
}

/// A single unit's fetch parameters, collected up front so all units can be fetched
/// concurrently rather than one-at-a-time.
struct UnitTask {
    gidx: usize,
    is_kc: bool,
    unit_uid: String,
    slug: Option<String>,
    base: Option<String>,
    module_url: String,
    position: usize,
    // 1-based index among the module's non-knowledge-check units, and that count, for the
    // ordinal include fallback.
    content_ordinal: usize,
    non_kc_count: usize,
    // Set when the module isn't in the public GitHub index but has a Learn URL: the page-URL
    // base to scrape this unit from instead of GitHub markdown.
    scrape_base: Option<String>,
}

/// The unit-page base for scraping a module that isn't on GitHub: the module's Learn training
/// URL, with the catalog tracking query and trailing slash removed. `None` if the module has no
/// usable training URL (nothing to scrape).
fn scrapeable_base(module_url: &str) -> Option<String> {
    if module_url.contains("/training/modules/") {
        Some(module_url.split('?').next().unwrap_or(module_url).trim_end_matches('/').to_string())
    } else {
        None
    }
}

/// Build an EPUB (bytes) for a single module identified by its uid.
///
/// `date_stamp` is `YYYY-MM-DD`; the caller supplies it so this stays free of any clock
/// dependency (native vs wasm).
pub async fn build_module_epub<F: Fetcher + Sync>(
    fetcher: &F,
    index: &ContentIndex,
    module_uid: &str,
    locale: &str,
    date_stamp: &str,
) -> Result<Vec<u8>, ResolveError> {
    let module = resolve_module(fetcher, module_uid, locale).await?;
    let module_url = module.url.clone().unwrap_or_default();
    let slug = module_slug_from_url(&module_url).ok_or_else(|| {
        ResolveError::BadInput(format!("module {module_uid} has no usable training URL"))
    })?;
    let folder_raw_base = index.module_raw_base(&slug).ok_or_else(|| {
        ResolveError::BadInput(format!(
            "module '{slug}' not found in the content index (rebuild the index?)"
        ))
    })?;

    let mut chapters: Vec<Chapter> = Vec::new();
    let mut sources: Vec<SourceRef> = Vec::new();

    // 1. Title page.
    chapters.push(Chapter {
        id: "ch000".into(),
        filename: "ch000.xhtml".into(),
        title: module.title.clone(),
        body: title_page_body(&module.title, &module_url, &module, date_stamp),
        module_header: None,
    });

    // 2. Units in order.
    let non_kc_count = module.units.iter().filter(|u| !u.is_knowledge_check).count();
    let mut content_ordinal = 0;
    for (i, unit) in module.units.iter().enumerate() {
        let n = i + 1;
        let (body, learn_url, raw_url) = if unit.is_knowledge_check {
            assemble_quiz(fetcher, index, &slug, &folder_raw_base, &module_url).await?
        } else {
            content_ordinal += 1;
            assemble_unit(
                fetcher, index, &slug, &folder_raw_base, &module_url, &unit.uid, n,
                content_ordinal, non_kc_count,
            )
            .await?
        };

        sources.push(SourceRef {
            title: unit.title.clone(),
            learn_url,
            raw_url,
        });
        chapters.push(Chapter {
            id: format!("ch{n:03}"),
            filename: format!("ch{n:03}.xhtml"),
            title: unit.title.clone(),
            body,
            module_header: None,
        });
    }

    // 3. Sources appendix.
    chapters.push(Chapter {
        id: "ch999".into(),
        filename: "ch999.xhtml".into(),
        title: "Sources and resources".into(),
        body: sources_body(&module.title, &module_url, &sources, date_stamp),
        module_header: None,
    });

    let resources = embed_images(fetcher, &mut chapters).await;
    let nav: Vec<NavEntry> = chapters
        .iter()
        .map(|c| NavEntry::leaf(c.filename.clone(), c.title.clone()))
        .collect();
    let identifier = format!("urn:mslx:{}:{}", module.uid, date_stamp);
    let modified = format!("{date_stamp}T00:00:00Z");
    let bytes = build_epub(
        &module.title,
        &identifier,
        &modified,
        "en",
        &chapters,
        &nav,
        &resources,
    )
    .map_err(|e| ResolveError::BadInput(format!("epub packaging failed: {e}")))?;
    Ok(bytes)
}

/// POC: build a module EPUB by **scraping** the rendered Learn unit pages instead of reading
/// GitHub Markdown. For modules whose source isn't in the public repo (the Microsoft 365 admin
/// track, etc.). Reuses the same Chapter / EPUB structure as [`build_module_epub`] so scraped
/// content lays onto the existing layout; only the per-unit body source differs. `progress` is
/// called per unit.
pub async fn build_scraped_module_epub<F: Fetcher + Sync>(
    fetcher: &F,
    module_uid: &str,
    locale: &str,
    date_stamp: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Vec<u8>, ResolveError> {
    let module = resolve_module(fetcher, module_uid, locale).await?;
    let module_url = module.url.clone().unwrap_or_default();
    // The page-URL base: strip the catalog tracking query and any trailing slash.
    let base = module_url.split('?').next().unwrap_or(&module_url).trim_end_matches('/');

    let mut chapters: Vec<Chapter> = Vec::new();
    let mut sources: Vec<SourceRef> = Vec::new();

    chapters.push(Chapter {
        id: "ch000".into(),
        filename: "ch000.xhtml".into(),
        title: module.title.clone(),
        body: title_page_body(&module.title, base, &module, date_stamp),
        module_header: None,
    });

    progress(&format!(
        "\"{}\" isn't in the public source; scraping its Microsoft Learn pages instead (slower).",
        module.title
    ));
    for (i, unit) in module.units.iter().enumerate() {
        let n = i + 1;
        let slug = unit_slug_from_uid(&unit.uid);
        progress(&format!("[{n}/{}] {} \u{203a} {}", module.units.len(), module.title, unit.title));
        let (body, learn_url) = scrape_unit_body(fetcher, base, n, slug, unit.is_knowledge_check).await;
        sources.push(SourceRef {
            title: unit.title.clone(),
            learn_url,
            raw_url: String::new(),
        });
        chapters.push(Chapter {
            id: format!("ch{n:03}"),
            filename: format!("ch{n:03}.xhtml"),
            title: unit.title.clone(),
            body,
            module_header: None,
        });
    }

    chapters.push(Chapter {
        id: "ch999".into(),
        filename: "ch999.xhtml".into(),
        title: "Sources and resources".into(),
        body: sources_body(&module.title, base, &sources, date_stamp),
        module_header: None,
    });

    let resources = embed_images(fetcher, &mut chapters).await;
    let nav: Vec<NavEntry> = chapters
        .iter()
        .map(|c| NavEntry::leaf(c.filename.clone(), c.title.clone()))
        .collect();
    let identifier = format!("urn:mslx:{}:{}", module.uid, date_stamp);
    let modified = format!("{date_stamp}T00:00:00Z");
    build_epub(
        &module.title,
        &identifier,
        &modified,
        "en",
        &chapters,
        &nav,
        &resources,
    )
    .map_err(|e| ResolveError::BadInput(format!("epub packaging failed: {e}")))
}

/// Fetch a unit page and extract its prose as XHTML. Tries the numbered URL (`{n}-{slug}`, the
/// canonical Learn form) and falls back to the bare slug; on any failure returns a placeholder
/// body so a single missing unit doesn't sink the whole module. Returns `(body, learn_url)`.
///
/// Knowledge-check units render their questions as an interactive widget that lives outside the
/// scraped content container, so the prose is just the intro. For those we append a link out to
/// the live page (mirrors the "Watch video on Microsoft Learn" treatment).
async fn scrape_unit_body<F: Fetcher>(
    fetcher: &F,
    base: &str,
    n: usize,
    slug: &str,
    is_kc: bool,
) -> (String, String) {
    let candidates = [format!("{base}/{n}-{slug}"), format!("{base}/{slug}")];
    for url in &candidates {
        // Learn serves each unit's original authored Markdown here, so it runs through the same
        // markdown_to_xhtml path as GitHub units (identical formatting).
        let md_url = format!("{url}?accept=text/markdown");
        if let Ok(md) = get_text_retrying(fetcher, &md_url).await {
            if let Some(xhtml) = crate::scrape::unit_markdown_to_xhtml(&md, url) {
                let body = if is_kc { with_kc_link(&xhtml, url) } else { xhtml };
                return (body, url.clone());
            }
        }
    }
    let url = &candidates[0];
    let body = if is_kc {
        with_kc_link("", url)
    } else {
        "<p class=\"muted\">This unit's content could not be scraped from Microsoft Learn.</p>".into()
    };
    (body, url.clone())
}

/// Append the "check your knowledge on Microsoft Learn" link to a knowledge-check body, since the
/// interactive quiz itself can't be scraped.
fn with_kc_link(intro_xhtml: &str, url: &str) -> String {
    format!(
        "{intro}<p><a href=\"{url}\">Check your knowledge on the Microsoft Learn website</a></p>",
        intro = intro_xhtml,
        url = esc(url),
    )
}

/// Whether an app input refers to a single module rather than a cert/exam/path - a
/// `/training/modules/<slug>` URL. Module *uids* aren't distinguished here (they overlap with
/// other uid shapes); URL is the reliable, user-facing signal.
pub fn input_is_module(input: &str) -> bool {
    input.contains("/training/modules/")
}

/// Top-level export entry: dispatch on the input. A module URL exports just that module
/// (GitHub markdown if indexed, else scraped); anything else is a full certification/exam/path.
pub async fn build_export_epub<F: Fetcher + Sync>(
    fetcher: &F,
    index: &ContentIndex,
    input: &str,
    locale: &str,
    date_stamp: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Vec<u8>, ResolveError> {
    if input_is_module(input) {
        build_module_epub_auto(fetcher, index, input, locale, date_stamp, progress).await
    } else {
        build_certification_epub(fetcher, index, input, locale, date_stamp, progress, false).await
    }
}

/// Build a single-module EPUB, choosing the source automatically: GitHub markdown when the module
/// is in the public index, otherwise scraping its Learn pages. `input` is a module uid or URL.
pub async fn build_module_epub_auto<F: Fetcher + Sync>(
    fetcher: &F,
    index: &ContentIndex,
    input: &str,
    locale: &str,
    date_stamp: &str,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<Vec<u8>, ResolveError> {
    let uid = resolve_module_uid(fetcher, input, locale).await?;
    let module = resolve_module(fetcher, &uid, locale).await?;
    let on_github = module
        .url
        .as_deref()
        .and_then(module_slug_from_url)
        .map(|s| index.module_raw_base(&s).is_some())
        .unwrap_or(false);
    if on_github {
        build_module_epub(fetcher, index, &module.uid, locale, date_stamp).await
    } else {
        build_scraped_module_epub(fetcher, &module.uid, locale, date_stamp, progress).await
    }
}

/// Resolve a module uid from a module uid or a `/training/modules/<slug>[/<unit>]` URL. The
/// catalog keys modules by uid (not slug), and uid != slug in general, so for a URL we read the
/// page's `<meta name="uid">`. A module landing page carries the module uid directly; a unit page
/// carries `<module-uid>.<unit-slug>`, so we drop the trailing unit segment in that case.
async fn resolve_module_uid<F: Fetcher>(
    fetcher: &F,
    input: &str,
    _locale: &str,
) -> Result<String, ResolveError> {
    if !input.contains("/training/modules/") {
        return Ok(input.to_string()); // already a uid
    }
    let clean = input.split('?').next().unwrap_or(input).trim_end_matches('/');
    // Segments after `/training/modules/`: `<module-slug>` alone, or `<module-slug>/<unit-stem>`.
    let is_unit_page = clean
        .split("/training/modules/")
        .nth(1)
        .map(|tail| tail.contains('/'))
        .unwrap_or(false);
    let html = get_text_retrying(fetcher, clean).await?;
    let uid = extract_meta_uid(&html)
        .ok_or_else(|| ResolveError::BadInput(format!("could not find a module uid at {input}")))?;
    Ok(if is_unit_page { drop_last_segment(&uid) } else { uid })
}

/// Pull `<meta name="uid" content="...">` from a Learn page.
fn extract_meta_uid(html: &str) -> Option<String> {
    let i = html.find("name=\"uid\"")?;
    let seg = &html[i..];
    let c = seg.find("content=\"")? + "content=\"".len();
    let end = seg[c..].find('"')?;
    Some(seg[c..c + end].to_string())
}

/// Drop the last dotted segment (`a.b.c` -> `a.b`). A unit-page uid is `<module-uid>.<unit-slug>`.
fn drop_last_segment(uid: &str) -> String {
    match uid.rfind('.') {
        Some(i) => uid[..i].to_string(),
        None => uid.to_string(),
    }
}

/// Build a single EPUB for an entire certification: parts -> modules -> units, with a
/// nested table of contents and a sources appendix. `progress` is called per module so a
/// CLI/UI can show movement during the ~200 content fetches.
pub async fn build_certification_epub<F: Fetcher + Sync>(
    fetcher: &F,
    index: &ContentIndex,
    input: &str,
    locale: &str,
    date_stamp: &str,
    progress: &dyn Fn(&str),
    // When true, source EVERY module by scraping Learn's markdown endpoint even if it's in the
    // public GitHub index. Used to A/B the two sources for parser tuning; normal builds pass false.
    force_scrape: bool,
) -> Result<Vec<u8>, ResolveError> {
    // A self-contained fixture book that exercises every element type, for checking rendering
    // across readers. No network needed, so it works in the browser too.
    if input.trim().eq_ignore_ascii_case("test-mslx-epub") {
        progress("Building the mslx test book\u{2026}");
        return build_test_epub(date_stamp);
    }
    let book = resolve_certification(fetcher, input, locale).await?;

    let mut chapters: Vec<Chapter> = Vec::new();
    let mut nav: Vec<NavEntry> = Vec::new();
    let mut tasks: Vec<UnitTask> = Vec::new();
    // Flat, ordered list of (gidx, module title, unit title) for the sources appendix.
    let mut source_slots: Vec<(usize, String, String)> = Vec::new();
    // gidx (1-based) -> index of its (empty-bodied) chapter, filled in after fetching.
    let mut chapter_of_gidx: Vec<usize> = vec![0];

    // Cover.
    chapters.push(Chapter {
        id: "cover".into(),
        filename: "cover.xhtml".into(),
        title: book.title.clone(),
        body: cover_body(&book, date_stamp),
        module_header: None,
    });
    nav.push(NavEntry::leaf("cover.xhtml", "Title page"));

    let mut gidx = 0usize;
    // Track how many modules actually have content in the public source repo. Some certs
    // resolve a full structure but none of their content is publicly available (Dynamics /
    // Business Central, Power Platform live in private repos with no public mirror).
    let mut modules_total = 0usize;
    let mut modules_with_source = 0usize;
    // Modules sourced by scraping rendered Learn pages (not on GitHub but reachable on Learn).
    let mut scraped_modules = 0usize;
    // Modules whose content is not in the public repo (placeholders in the book), reported at
    // the end so the page can warn the user and log which parts were unavailable.
    let mut missing_modules: Vec<String> = Vec::new();

    // Pass 1: walk the tree, build the structure + a flat list of fetch tasks (no IO yet).
    for (pi, part) in book.parts.iter().enumerate() {
        let part_file = format!("part{:02}.xhtml", pi + 1);
        let part_label = format!("Part {}: {}", pi + 1, part.title);
        chapters.push(Chapter {
            id: format!("part{:02}", pi + 1),
            filename: part_file.clone(),
            title: part_label.clone(),
            body: part_intro_body(pi + 1, part),
            module_header: None,
        });
        let mut part_nav = NavEntry::leaf(part_file.clone(), part_label);

        for module in &part.modules {
            let module_url = module.url.clone().unwrap_or_default();
            let slug = module_slug_from_url(&module_url);
            let github_base = slug.as_deref().and_then(|s| index.module_raw_base(s));
            // Not on GitHub (or forced) but reachable on Learn -> scrape the markdown endpoint.
            let scrape_base = if github_base.is_none() || force_scrape {
                scrapeable_base(&module_url)
            } else {
                None
            };
            // When forcing the scrape, ignore the GitHub source for this module.
            let base = if force_scrape && scrape_base.is_some() { None } else { github_base };
            modules_total += 1;
            if base.is_some() {
                modules_with_source += 1;
            } else if scrape_base.is_some() {
                modules_with_source += 1;
                scraped_modules += 1;
            } else {
                missing_modules.push(module.title.clone());
            }

            let mut module_nav = NavEntry::leaf(part_file.clone(), module.title.clone());
            let mut first_unit_file: Option<String> = None;
            let non_kc_count = module.units.iter().filter(|u| !u.is_knowledge_check).count();
            let mut content_ordinal = 0;

            for (ui, unit) in module.units.iter().enumerate() {
                gidx += 1;
                let file = format!("u{gidx:04}.xhtml");
                if !unit.is_knowledge_check {
                    content_ordinal += 1;
                }
                tasks.push(UnitTask {
                    gidx,
                    is_kc: unit.is_knowledge_check,
                    unit_uid: unit.uid.clone(),
                    slug: slug.clone(),
                    base: base.clone(),
                    module_url: module_url.clone(),
                    position: ui + 1,
                    content_ordinal,
                    non_kc_count,
                    scrape_base: scrape_base.clone(),
                });
                source_slots.push((gidx, module.title.clone(), unit.title.clone()));
                module_nav
                    .children
                    .push(NavEntry::leaf(file.clone(), unit.title.clone()));
                chapters.push(Chapter {
                    id: format!("u{gidx:04}"),
                    filename: file.clone(),
                    title: unit.title.clone(),
                    body: String::new(), // filled after concurrent fetch
                    // Show the module name above the first unit (usually "Introduction").
                    module_header: if ui == 0 {
                        Some(module.title.clone())
                    } else {
                        None
                    },
                });
                chapter_of_gidx.push(chapters.len() - 1);
                if first_unit_file.is_none() {
                    first_unit_file = Some(file);
                }
            }

            module_nav.href = first_unit_file.unwrap_or_else(|| part_file.clone());
            part_nav.children.push(module_nav);
        }
        nav.push(part_nav);
    }

    // If the certification has little or no publicly-sourced content, don't fetch hundreds of
    // empty units or hand back a hollow book - tell the caller plainly. This covers fully
    // private tracks (Dynamics / Power Platform) and ones where only a minority of modules are
    // public, e.g. the Microsoft 365 admin track (MS-900 retired, MS-102 active): their
    // security modules resolve from the shared public groups, but the admin/fundamentals
    // modules are not in the public mirror, so the book would be mostly empty.
    if is_bulk_unavailable(modules_with_source, modules_total) {
        return Err(ResolveError::ContentUnavailable(book.title.clone()));
    }

    // Pass 2: fetch all unit content concurrently (bounded), filling chapter bodies as each
    // arrives so the caller's `progress` can stream "[n/total] <module> > <unit>" live.
    let total = tasks.len();
    // Throttle when scraping: learn.microsoft.com rate-limits a heavy burst (GitHub-raw doesn't),
    // so drop to a gentler concurrency whenever any module is sourced from the scrape path.
    let concurrency = if scraped_modules > 0 { SCRAPE_CONCURRENCY } else { FETCH_CONCURRENCY };
    // gidx -> "<module> > <unit>" for the per-item progress line.
    let unit_label: HashMap<usize, String> = source_slots
        .iter()
        .map(|(g, m, u)| (*g, format!("{m} \u{203a} {u}")))
        .collect();
    if scraped_modules > 0 {
        progress(&format!(
            "{scraped_modules} module(s) aren't in the public source; scraping their Microsoft \
             Learn pages instead (this is slower)."
        ));
    }
    progress(&format!("Fetching {total} units\u{2026}"));
    let fetches = stream::iter(tasks)
        .map(|t| async move {
            // Scraped module: pull each unit's prose from its rendered Learn page.
            if let Some(sbase) = t.scrape_base.as_deref() {
                let uslug = unit_slug_from_uid(&t.unit_uid);
                let (body, learn) =
                    scrape_unit_body(fetcher, sbase, t.position, uslug, t.is_kc).await;
                return (t.gidx, (body, learn, String::new()));
            }
            let r = match (t.slug.as_deref(), t.base.as_deref()) {
                (Some(slug), Some(base)) => {
                    let res = if t.is_kc {
                        assemble_quiz(fetcher, index, slug, base, &t.module_url).await
                    } else {
                        assemble_unit(
                            fetcher, index, slug, base, &t.module_url, &t.unit_uid, t.position,
                            t.content_ordinal, t.non_kc_count,
                        )
                        .await
                    };
                    res.unwrap_or_else(|_| {
                        (
                            "<p class=\"muted\">Content could not be fetched.</p>".into(),
                            t.module_url.clone(),
                            String::new(),
                        )
                    })
                }
                _ => (
                    "<p class=\"muted\">Content source not found for this module.</p>".into(),
                    t.module_url.clone(),
                    String::new(),
                ),
            };
            (t.gidx, r)
        })
        .buffer_unordered(concurrency);
    futures::pin_mut!(fetches);
    let mut results: HashMap<usize, (String, String, String)> = HashMap::new();
    let mut done = 0usize;
    while let Some((gidx, r)) = fetches.next().await {
        done += 1;
        if let Some(label) = unit_label.get(&gidx) {
            progress(&format!("[{done}/{total}] {label}"));
        }
        results.insert(gidx, r);
    }

    for (g, slot) in chapter_of_gidx.iter().enumerate().skip(1) {
        if let Some((body, _, _)) = results.get(&g) {
            chapters[*slot].body = body.clone();
        }
    }

    // Sources appendix, grouped by module in reading order, with a small provenance line at
    // the very end so a reader can find the project (and report issues).
    let mut sources_body = cert_sources_body_flat(&book, &source_slots, &results, date_stamp);
    sources_body.push_str(&format!(
        "<hr/>\n<p class=\"muted\">Made with mslx. Source and issues at \
         <a href=\"https://{repo}\">{repo}</a>.</p>\n",
        repo = MSLX_REPO,
    ));
    chapters.push(Chapter {
        id: "sources".into(),
        filename: "sources.xhtml".into(),
        title: "Sources and resources".into(),
        body: sources_body,
        module_header: None,
    });
    nav.push(NavEntry::leaf("sources.xhtml", "Sources and resources"));

    progress("Downloading and embedding images\u{2026}");
    let resources = embed_images(fetcher, &mut chapters).await;

    let identifier = format!("urn:mslx:{}:{}", book.cert_uid, date_stamp);
    let modified = format!("{date_stamp}T00:00:00Z");
    progress("Packaging the EPUB\u{2026}");
    let bytes = build_epub(
        &book.title,
        &identifier,
        &modified,
        "en",
        &chapters,
        &nav,
        &resources,
    )
    .map_err(|e| ResolveError::BadInput(format!("epub packaging failed: {e}")))?;

    // Report which modules had no public source and which individual units rendered a
    // placeholder (no match / fetch failed), marker-prefixed so the page intercepts it from the
    // progress stream (for a warning + telemetry) rather than displaying it.
    let missing_json = missing_modules
        .iter()
        .map(|m| format!("\"{}\"", json_escape(m)))
        .collect::<Vec<_>>()
        .join(",");
    let unit_placeholders: Vec<String> = source_slots
        .iter()
        .filter_map(|(g, module, unit)| {
            let (body, _, _) = results.get(g)?;
            (body.contains("could not be fetched")
                || body.contains("not available for this unit")
                || body.contains("could not be scraped"))
                .then(|| format!("{module} / {unit}"))
        })
        .collect();
    let placeholders_json = unit_placeholders
        .iter()
        .map(|p| format!("\"{}\"", json_escape(p)))
        .collect::<Vec<_>>()
        .join(",");
    progress(&format!(
        "__MSLX_REPORT__{{\"resolved\":\"{}\",\"missing\":[{}],\"unitPlaceholders\":[{}]}}",
        json_escape(&book.title),
        missing_json,
        placeholders_json
    ));

    Ok(bytes)
}

/// Minimal JSON string escaping for the progress report payload.
fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
        .replace('\r', " ")
        .replace('\t', " ")
}

/// A centered badge `<img>` (the SVG url; `embed_images` rasterizes it to PNG so it renders
/// in every reader).
fn badge_img(icon_url: &Option<String>) -> String {
    match icon_url {
        Some(u) if u.starts_with("http") => format!(
            "<p class=\"badge-wrap\"><img class=\"badge\" src=\"{}\" alt=\"badge\"/></p>\n",
            esc(u)
        ),
        _ => String::new(),
    }
}

/// A self-contained "kitchen sink" EPUB exercising every element mslx emits: a cover badge,
/// nested TOC, headings, lists, a table, a blockquote, footnote, inline + block code, a
/// rasterized SVG diagram and figure, and a knowledge-check quiz with an answer key. Built
/// from the input `test-mslx-epub`; handy for checking rendering across readers. No network.
pub fn build_test_epub(date_stamp: &str) -> Result<Vec<u8>, ResolveError> {
    use crate::quiz::{Choice, Question, Quiz};

    let mut chapters: Vec<Chapter> = Vec::new();
    let mut nav: Vec<NavEntry> = Vec::new();
    let mut resources: Vec<Resource> = Vec::new();

    let push_png = |resources: &mut Vec<Resource>, name: &str, svg: &str| {
        if let Some(png) = rasterize_svg(svg) {
            resources.push(Resource {
                filename: format!("media/{name}.png"),
                media_type: "image/png".to_string(),
                data: png,
            });
        }
    };

    // Cover: badge + description (the title comes from the chapter wrapper).
    push_png(&mut resources, "badge", TEST_BADGE_SVG);
    let cover_body = format!(
        "<div class=\"badge-wrap\"><img class=\"badge\" src=\"media/badge.png\" alt=\"mslx test badge\"/></div>\n\
         <p class=\"muted\">A fixture that exercises every element mslx emits, so you can see how a \
         reader renders headings, lists, tables, blockquotes, inline and block code, images, an SVG \
         diagram, and a knowledge-check quiz. No Microsoft content. Assembled {date}.</p>",
        date = esc(date_stamp),
    );
    chapters.push(Chapter {
        id: "cover".into(),
        filename: "cover.xhtml".into(),
        title: "mslx rendering test book".into(),
        body: cover_body,
        module_header: None,
    });
    nav.push(NavEntry::leaf("cover.xhtml", "Title page"));

    // Part 1 (nested in the TOC) with one unit per element family.
    let mut part = NavEntry::leaf("u01.xhtml", "Part 1: Elements");

    chapters.push(Chapter {
        id: "u01".into(),
        filename: "u01.xhtml".into(),
        title: "Text and structure".into(),
        body: crate::markdown::markdown_to_xhtml(TEST_TEXT_MD, "media"),
        module_header: Some("Part 1: Elements".into()),
    });
    part.children.push(NavEntry::leaf("u01.xhtml", "Text and structure"));

    chapters.push(Chapter {
        id: "u02".into(),
        filename: "u02.xhtml".into(),
        title: "Code blocks".into(),
        body: crate::markdown::markdown_to_xhtml(TEST_CODE_MD, "media"),
        module_header: None,
    });
    part.children.push(NavEntry::leaf("u02.xhtml", "Code blocks"));

    push_png(&mut resources, "diagram", TEST_DIAGRAM_SVG);
    push_png(&mut resources, "figure", TEST_FIGURE_SVG);
    chapters.push(Chapter {
        id: "u03".into(),
        filename: "u03.xhtml".into(),
        title: "Images and diagrams".into(),
        body: TEST_IMAGE_BODY.to_string(),
        module_header: None,
    });
    part.children.push(NavEntry::leaf("u03.xhtml", "Images and diagrams"));

    let quiz = Quiz {
        title: String::new(),
        questions: vec![
            Question {
                content: "Which command creates a resource group?".into(),
                choices: vec![
                    Choice { content: "az group create".into(), is_correct: true, explanation: "az group create makes a new resource group.".into() },
                    Choice { content: "az vm create".into(), is_correct: false, explanation: String::new() },
                    Choice { content: "az storage account create".into(), is_correct: false, explanation: String::new() },
                ],
            },
            Question {
                content: "Which of these are Azure compute services? (Choose two.)".into(),
                choices: vec![
                    Choice { content: "Virtual Machines".into(), is_correct: true, explanation: String::new() },
                    Choice { content: "Blob Storage".into(), is_correct: false, explanation: String::new() },
                    Choice { content: "Azure Functions".into(), is_correct: true, explanation: "Virtual Machines and Functions both run your workloads.".into() },
                ],
            },
        ],
    };
    chapters.push(Chapter {
        id: "u04".into(),
        filename: "u04.xhtml".into(),
        title: "Knowledge check".into(),
        body: render_quiz_xhtml(&quiz),
        module_header: None,
    });
    part.children.push(NavEntry::leaf("u04.xhtml", "Knowledge check"));

    nav.push(part);

    // Sources appendix + the standard provenance line.
    let sources_body = format!(
        "<p>This is a synthetic test book generated by mslx to check element rendering. It \
         contains no Microsoft Learn content.</p>\n\
         <ul>\n<li><a href=\"https://learn.microsoft.com/\">Microsoft Learn</a></li>\n\
         <li><a href=\"https://{repo}\">mslx on GitHub</a></li>\n</ul>\n\
         <hr/>\n<p class=\"muted\">Made with mslx. Source and issues at \
         <a href=\"https://{repo}\">{repo}</a>.</p>\n",
        repo = MSLX_REPO,
    );
    chapters.push(Chapter {
        id: "sources".into(),
        filename: "sources.xhtml".into(),
        title: "Sources and resources".into(),
        body: sources_body,
        module_header: None,
    });
    nav.push(NavEntry::leaf("sources.xhtml", "Sources and resources"));

    let modified = format!("{date_stamp}T00:00:00Z");
    build_epub(
        "mslx rendering test book",
        "mslx-test-book",
        &modified,
        "en",
        &chapters,
        &nav,
        &resources,
    )
    .map_err(|e| ResolveError::BadInput(format!("epub packaging failed: {e}")))
}

const TEST_BADGE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="220" height="220" viewBox="0 0 220 220">
<path d="M110 12 L196 44 L196 120 Q196 184 110 208 Q24 184 24 120 L24 44 Z" fill="#0b3d5c"/>
<text x="110" y="100" font-family="sans-serif" font-size="30" font-weight="bold" fill="#ffffff" text-anchor="middle">mslx</text>
<text x="110" y="138" font-family="sans-serif" font-size="18" fill="#bcd6ea" text-anchor="middle">TEST</text>
</svg>"##;

const TEST_DIAGRAM_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="520" height="180" viewBox="0 0 520 180">
<rect x="20" y="60" width="150" height="60" rx="8" fill="#e8f0f7" stroke="#0b3d5c" stroke-width="2"/>
<text x="95" y="96" font-family="sans-serif" font-size="18" fill="#0b3d5c" text-anchor="middle">Client</text>
<rect x="350" y="60" width="150" height="60" rx="8" fill="#e8f0f7" stroke="#0b3d5c" stroke-width="2"/>
<text x="425" y="96" font-family="sans-serif" font-size="18" fill="#0b3d5c" text-anchor="middle">Server</text>
<line x1="170" y1="90" x2="345" y2="90" stroke="#0b3d5c" stroke-width="2"/>
<polygon points="345,90 333,84 333,96" fill="#0b3d5c"/>
<text x="258" y="78" font-family="sans-serif" font-size="14" fill="#33526a" text-anchor="middle">HTTPS request</text>
</svg>"##;

const TEST_FIGURE_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="440" height="220" viewBox="0 0 440 220">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#4f8cc9"/><stop offset="1" stop-color="#0b3d5c"/></linearGradient></defs>
<rect width="440" height="220" fill="url(#g)"/>
<circle cx="130" cy="110" r="56" fill="#ffd24d"/>
<rect x="220" y="70" width="170" height="80" rx="10" fill="#ffffff" opacity="0.92"/>
<text x="305" y="118" font-family="sans-serif" font-size="22" fill="#0b3d5c" text-anchor="middle">Sample figure</text>
</svg>"##;

const TEST_IMAGE_BODY: &str = r##"<p>A rasterized SVG diagram. This tests inline SVG text rendering, which many EPUB readers cannot do natively but mslx bakes in:</p>
<img class="content-img" src="media/diagram.png" alt="Client to server request diagram"/>
<p>A figure with a gradient, shapes, and a label:</p>
<img class="content-img" src="media/figure.png" alt="Sample figure"/>
"##;

const TEST_TEXT_MD: &str = r##"This unit shows the text and structural elements. This paragraph mixes **bold**, *italic*, ~~strikethrough~~, and `inline code` such as `az group list`, plus a [link to Microsoft Learn](https://learn.microsoft.com/).

### Unordered and nested lists

- First item
- Second item
  - Nested item A
  - Nested item B
- Third item

### Ordered list

1. Create a resource group.
2. Deploy the template.
3. Verify the deployment.

### Table

| Resource | SKU | Region |
| --- | --- | --- |
| Storage account | Standard_LRS | westus2 |
| Virtual machine | Standard_DS1_v2 | eastus |
| Key vault | Standard | westeurope |

### Blockquote

> **Note** Blockquotes are used for notes, tips, and warnings throughout Microsoft Learn content.

A footnote reference sits here.[^1]

[^1]: This is the footnote text, rendered at the end of the section.
"##;

const TEST_CODE_MD: &str = r##"This unit shows code rendering. Inline commands like `az login` and parameters like `--resource-group` appear in prose.

### Long single-line command

```bash
RGROUP=$(az group create --name vmbackups --location westus2 --output tsv --query name)
```

### Multi-line command

```bash
az vm create \
    --resource-group $RGROUP \
    --name NW-APP01 \
    --image Win2025Datacenter \
    --admin-username azureuser \
    --generate-ssh-keys
```

### JSON

```json
{
  "parameters": {
    "storageName": { "type": "string", "minLength": 3, "maxLength": 24 }
  }
}
```

### PowerShell

```powershell
New-AzResourceGroup -Name "vmbackups" -Location "westus2"
```
"##;

fn cover_body(book: &Book, date_stamp: &str) -> String {
    // Label the cover by what was actually resolved (the identifier prefix encodes the type).
    let kind = if book.cert_uid.starts_with("exam.") {
        "Exam"
    } else if book.cert_uid.starts_with("certification.") {
        "Certification"
    } else {
        "Learning path"
    };
    badge_img(&book.icon_url)
        + &format!(
        "<p class=\"muted\">Microsoft Learn study export, assembled {date} for personal study. \
         Content is Microsoft's; original sources are listed at the end.</p>\n\
         <p><strong>{kind}:</strong> {title}</p>\n\
         <p><strong>{parts} parts &#183; {modules} modules &#183; {units} units &#183; about {hours:.0} hours</strong></p>\n",
        date = esc(date_stamp),
        title = esc(&book.title),
        parts = book.parts.len(),
        modules = book.module_count(),
        units = book.unit_count(),
        hours = book.total_minutes() as f64 / 60.0,
    )
}

fn part_intro_body(n: usize, part: &crate::Part) -> String {
    let mut s = badge_img(&part.icon_url);
    s.push_str(&format!(
        "<p class=\"muted\">Part {n}</p>\n<p><strong>Modules in this part:</strong></p>\n<ul>\n"
    ));
    for m in &part.modules {
        s.push_str(&format!(
            "  <li>{} <span class=\"muted\">(about {} min)</span></li>\n",
            esc(&m.title),
            m.duration_in_minutes.unwrap_or(0)
        ));
    }
    s.push_str("</ul>\n");
    s
}

/// Build the sources appendix from the flat, ordered unit slots and the fetch results,
/// emitting a module heading whenever the module changes.
fn cert_sources_body_flat(
    book: &Book,
    slots: &[(usize, String, String)],
    results: &HashMap<usize, (String, String, String)>,
    date_stamp: &str,
) -> String {
    let mut s = format!(
        "<p>Assembled from <a href=\"https://learn.microsoft.com/credentials/certifications/\">{title}</a> \
         on {date}. Original Learn pages and content sources, by module:</p>\n",
        title = esc(&book.title),
        date = esc(date_stamp)
    );
    let mut current_module: Option<&str> = None;
    let mut open_list = false;
    for (gidx, module_title, unit_title) in slots {
        if current_module != Some(module_title.as_str()) {
            if open_list {
                s.push_str("</ol>\n");
            }
            s.push_str(&format!("<h3>{}</h3>\n<ol>\n", esc(module_title)));
            current_module = Some(module_title.as_str());
            open_list = true;
        }
        let (learn_url, raw_url) = results
            .get(gidx)
            .map(|(_, l, r)| (l.as_str(), r.as_str()))
            .unwrap_or(("", ""));
        s.push_str(&format!(
            "  <li>{t}<br/>\n    <a href=\"{lu}\">{lu}</a>",
            t = esc(unit_title),
            lu = esc(learn_url)
        ));
        if !raw_url.is_empty() {
            s.push_str(&format!(
                "<br/>\n    <span class=\"muted\">source: <a href=\"{ru}\">{ru}</a></span>",
                ru = esc(raw_url)
            ));
        }
        s.push_str("</li>\n");
    }
    if open_list {
        s.push_str("</ol>\n");
    }
    s
}

/// Whether a book whose modules are `with_source`-of-`total` publicly sourced should be reported
/// as content-unavailable instead of built. Blocks when content is absent or a minority (fewer
/// than half the modules), so a normal certification missing one or two modules still builds, but
/// a mostly-private track (MS-900 / MS-102 admin content, Dynamics, Power Platform) gives the
/// caller a clear "not publicly available" signal rather than a hollow EPUB.
fn is_bulk_unavailable(with_source: usize, total: usize) -> bool {
    total > 0 && with_source * 2 < total
}

/// Fetch text with a couple of immediate retries. The unit loop runs many concurrent fetches;
/// raw.githubusercontent.com occasionally drops one under load, and a silent retry avoids a
/// whole unit rendering as "Content could not be fetched" for a transient blip.
async fn get_text_retrying<F: Fetcher>(fetcher: &F, url: &str) -> Result<String, crate::FetchError> {
    let mut last = None;
    for attempt in 0..4 {
        // Back off before every retry (200ms, 400ms, 800ms). Immediate retries were useless
        // against a brief GitHub-raw rate-limit/reset under the concurrent fetch burst - all
        // three fired in the same instant and failed together, leaving a placeholder unit.
        if attempt > 0 {
            fetcher.sleep_ms(200u64 << (attempt - 1)).await;
        }
        match fetcher.get_json(url).await {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("loop runs at least once"))
}

/// Same as [`get_text_retrying`] for binary assets. A dropped image fetch otherwise leaves the
/// `<img>` pointing at the remote URL, which fails offline (the whole point of the export).
async fn get_bytes_retrying<F: Fetcher>(fetcher: &F, url: &str) -> Result<Vec<u8>, crate::FetchError> {
    let mut last = None;
    for attempt in 0..4 {
        if attempt > 0 {
            fetcher.sleep_ms(200u64 << (attempt - 1)).await;
        }
        match fetcher.get_bytes(url).await {
            Ok(b) => return Ok(b),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("loop runs at least once"))
}

async fn assemble_unit<F: Fetcher>(
    fetcher: &F,
    index: &ContentIndex,
    slug: &str,
    folder_raw_base: &str,
    module_url: &str,
    unit_uid: &str,
    position: usize,
    content_ordinal: usize,
    non_kc_count: usize,
) -> Result<(String, String, String), ResolveError> {
    let unit_slug = unit_slug_from_uid(unit_uid);
    // Primary: match by uid slug. Fallbacks for units whose uid slug differs from the file
    // slug: the file numbered `position`, then the content-ordinal mapping (Nth non-KC unit ->
    // Nth content file) which also survives numbering gaps and generic file names.
    let file = index
        .unit_include_file(slug, unit_slug)
        .or_else(|| index.unit_include_file_by_number(slug, position))
        .or_else(|| index.unit_include_file_by_ordinal(slug, content_ordinal, non_kc_count));
    match file {
        Some(file) => {
            let url = format!("{folder_raw_base}/includes/{file}");
            let md = get_text_retrying(fetcher, &url).await?;
            let stem = file.trim_end_matches(".md");
            let learn_url = format!("{}/{}", module_url.trim_end_matches('/'), stem);
            let body = markdown_to_xhtml_with_unit(&md, folder_raw_base, &learn_url);
            Ok((body, learn_url, url))
        }
        None => Ok((
            "<p class=\"muted\">Content not available for this unit.</p>".into(),
            module_url.to_string(),
            String::new(),
        )),
    }
}

async fn assemble_quiz<F: Fetcher>(
    fetcher: &F,
    index: &ContentIndex,
    slug: &str,
    folder_raw_base: &str,
    module_url: &str,
) -> Result<(String, String, String), ResolveError> {
    let Some(kc_file) = index.knowledge_check_yml(slug) else {
        return Ok((
            "<p class=\"muted\">No knowledge check found for this module.</p>".into(),
            module_url.to_string(),
            String::new(),
        ));
    };
    let raw_url = format!("{folder_raw_base}/{kc_file}");
    let yaml = fetcher.get_json(&raw_url).await?;
    let body = match quiz::parse_quiz(&yaml) {
        Ok(Some(q)) => render_quiz_xhtml(&q),
        _ => "<p class=\"muted\">Knowledge check could not be parsed.</p>".into(),
    };
    let stem = kc_file.trim_end_matches(".yml");
    let learn_url = format!("{}/{}", module_url.trim_end_matches('/'), stem);
    Ok((body, learn_url, raw_url))
}

fn render_quiz_xhtml(quiz: &Quiz) -> String {
    // Questions first, then a separate "Answer key" section at the end of the unit. Unlike a
    // CSS rotate/transform (silently ignored by many readers, which then spoils the answer),
    // plain text placement is universally reader-compatible.
    let mut s = String::from(
        "<p class=\"muted\">Self-check. Correct answers are in the answer key at the end of this section.</p>\n",
    );
    for (i, q) in quiz.questions.iter().enumerate() {
        s.push_str(&format!(
            "<div class=\"quiz-q\">\n<p><strong>Q{}.</strong> {}</p>\n",
            i + 1,
            esc(&q.content)
        ));
        // Choices as a lettered list (A, B, C ... via CSS), with no answer highlighted.
        s.push_str("<ol class=\"quiz-choices\">\n");
        for c in &q.choices {
            s.push_str(&format!("  <li>{}</li>\n", esc(&c.content)));
        }
        s.push_str("</ol>\n");
        s.push_str("</div>\n");
    }
    // Answer key, well below the questions so answers are not given away at a glance.
    s.push_str("<hr/>\n<h3 class=\"answer-key\">Answer key</h3>\n");
    for (i, q) in quiz.questions.iter().enumerate() {
        s.push_str(&format!(
            "<p class=\"answer\"><strong>Q{}.</strong> {}</p>\n",
            i + 1,
            answer_html(q)
        ));
    }
    s
}

/// Build the (escaped) "Answer: B - explanation" string for a question, using letters that
/// match the lettered choice list.
fn answer_html(q: &Question) -> String {
    let mut letters = Vec::new();
    let mut expls = Vec::new();
    for (idx, c) in q.choices.iter().enumerate() {
        if c.is_correct {
            letters.push(letter_for(idx));
            if !c.explanation.is_empty() {
                expls.push(c.explanation.clone());
            }
        }
    }
    let label = if letters.len() > 1 { "Answers" } else { "Answer" };
    let letters_str = esc(&letters.join(", "));
    let expl = expls.join(" ");
    if expl.is_empty() {
        format!("{label}: {letters_str}")
    } else {
        format!("{label}: {letters_str} - {}", esc(&expl))
    }
}

fn letter_for(idx: usize) -> String {
    if idx < 26 {
        ((b'A' + idx as u8) as char).to_string()
    } else {
        (idx + 1).to_string()
    }
}

fn title_page_body(
    title: &str,
    module_url: &str,
    module: &crate::ModuleNode,
    date_stamp: &str,
) -> String {
    let dur = module.duration_in_minutes.unwrap_or(0);
    format!(
        "<p class=\"muted\">Microsoft Learn study export, assembled {date} for personal study. \
         Content is Microsoft's; original sources are listed at the end.</p>\n\
         <p><strong>Module:</strong> {title}</p>\n\
         <p><strong>Source:</strong> <a href=\"{url}\">{url}</a></p>\n\
         <p><strong>Units:</strong> {units} &#183; about {dur} min</p>\n",
        date = esc(date_stamp),
        title = esc(title),
        url = esc(module_url),
        units = module.units.len(),
        dur = dur
    )
}

fn sources_body(
    title: &str,
    module_url: &str,
    sources: &[SourceRef],
    date_stamp: &str,
) -> String {
    let mut s = format!(
        "<p>Assembled from <a href=\"{url}\">{title}</a> on {date}. \
         Each unit's original Learn page and content source:</p>\n<ol>\n",
        url = esc(module_url),
        title = esc(title),
        date = esc(date_stamp)
    );
    for src in sources {
        s.push_str(&format!(
            "  <li>{t}<br/>\n    <a href=\"{lu}\">{lu}</a>",
            t = esc(&src.title),
            lu = esc(&src.learn_url)
        ));
        if !src.raw_url.is_empty() {
            s.push_str(&format!(
                "<br/>\n    <span class=\"muted\">source: <a href=\"{ru}\">{ru}</a></span>",
                ru = esc(&src.raw_url)
            ));
        }
        s.push_str("</li>\n");
    }
    s.push_str("</ol>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Book;

    fn book(id: &str) -> Book {
        Book {
            cert_uid: id.into(),
            title: "Title".into(),
            icon_url: None,
            parts: vec![],
        }
    }

    #[test]
    fn cover_label_reflects_resolved_type() {
        assert!(cover_body(&book("certification.azure-fundamentals"), "2026-01-01")
            .contains("Certification:"));
        assert!(cover_body(&book("exam.ai-900"), "2026-01-01").contains("Exam:"));
        assert!(cover_body(&book("learn.wwl.well-architected"), "2026-01-01")
            .contains("Learning path:"));
    }

    // A Fetcher that fails its first `fail_n` get_json calls then succeeds, recording every
    // backoff sleep. Atomics/Mutex keep the futures Send (the native trait bound requires it).
    struct FlakyFetcher {
        remaining_failures: std::sync::atomic::AtomicUsize,
        sleeps: std::sync::Mutex<Vec<u64>>,
    }

    #[async_trait::async_trait]
    impl Fetcher for FlakyFetcher {
        async fn get_json(&self, url: &str) -> Result<String, crate::FetchError> {
            use std::sync::atomic::Ordering;
            if self.remaining_failures.load(Ordering::SeqCst) > 0 {
                self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
                return Err(crate::FetchError { url: url.into(), message: "flaky".into() });
            }
            Ok("body".into())
        }
        async fn get_bytes(&self, _url: &str) -> Result<Vec<u8>, crate::FetchError> {
            Ok(vec![])
        }
        async fn sleep_ms(&self, ms: u64) {
            self.sleeps.lock().unwrap().push(ms);
        }
    }

    #[test]
    fn bulk_unavailable_blocks_mostly_private_certs_only() {
        // Fully private or empty -> block.
        assert!(is_bulk_unavailable(0, 10));
        assert!(is_bulk_unavailable(0, 1));
        // MS-900-style: only the shared security modules resolve (3 of 10) -> block.
        assert!(is_bulk_unavailable(3, 10));
        // Minority content (4 of 10) -> block.
        assert!(is_bulk_unavailable(4, 10));
        // Exactly half or a healthy majority -> build (a normal cert missing a module or two).
        assert!(!is_bulk_unavailable(5, 10));
        assert!(!is_bulk_unavailable(28, 30));
        assert!(!is_bulk_unavailable(1, 1));
        // No modules at all (nothing resolved) is handled elsewhere, not flagged here.
        assert!(!is_bulk_unavailable(0, 0));
    }

    #[test]
    fn retry_recovers_with_exponential_backoff() {
        // Two transient failures then success: the helper must return Ok and have slept twice
        // with growing delays (200ms, 400ms) - the SC-900 "could not be fetched" placeholders
        // were these same transient bursts, where zero-delay retries all failed together.
        let f = FlakyFetcher {
            remaining_failures: std::sync::atomic::AtomicUsize::new(2),
            sleeps: std::sync::Mutex::new(vec![]),
        };
        let r = futures::executor::block_on(get_text_retrying(&f, "https://example/x.md"));
        assert_eq!(r.unwrap(), "body");
        assert_eq!(*f.sleeps.lock().unwrap(), vec![200, 400]);
    }

    #[test]
    fn retry_gives_up_after_four_attempts() {
        // Never-succeeding: 4 attempts -> 3 backoff sleeps (200, 400, 800), then Err.
        let f = FlakyFetcher {
            remaining_failures: std::sync::atomic::AtomicUsize::new(usize::MAX),
            sleeps: std::sync::Mutex::new(vec![]),
        };
        let r = futures::executor::block_on(get_text_retrying(&f, "https://example/x.md"));
        assert!(r.is_err());
        assert_eq!(*f.sleeps.lock().unwrap(), vec![200, 400, 800]);
    }
}
