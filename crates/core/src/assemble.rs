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
                    match fetcher.get_json(&url).await {
                        Ok(svg) => rasterize_svg(&svg).map(|png| ("png".to_string(), png)),
                        Err(_) => None,
                    }
                } else {
                    fetcher
                        .get_bytes(&url)
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
    for (i, unit) in module.units.iter().enumerate() {
        let n = i + 1;
        let (body, learn_url, raw_url) = if unit.is_knowledge_check {
            assemble_quiz(fetcher, index, &slug, &folder_raw_base, &module_url).await?
        } else {
            assemble_unit(fetcher, index, &slug, &folder_raw_base, &module_url, &unit.uid, n)
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
) -> Result<Vec<u8>, ResolveError> {
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
            let base = slug.as_deref().and_then(|s| index.module_raw_base(s));
            modules_total += 1;
            if base.is_some() {
                modules_with_source += 1;
            }

            let mut module_nav = NavEntry::leaf(part_file.clone(), module.title.clone());
            let mut first_unit_file: Option<String> = None;

            for (ui, unit) in module.units.iter().enumerate() {
                gidx += 1;
                let file = format!("u{gidx:04}.xhtml");
                tasks.push(UnitTask {
                    gidx,
                    is_kc: unit.is_knowledge_check,
                    unit_uid: unit.uid.clone(),
                    slug: slug.clone(),
                    base: base.clone(),
                    module_url: module_url.clone(),
                    position: ui + 1,
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

    // If the whole certification has no publicly-sourced content, don't fetch hundreds of
    // empty units or hand back a hollow book - tell the caller plainly.
    if modules_total > 0 && modules_with_source == 0 {
        return Err(ResolveError::ContentUnavailable(book.title.clone()));
    }

    // Pass 2: fetch all unit content concurrently (bounded), filling chapter bodies as each
    // arrives so the caller's `progress` can stream "[n/total] <module> > <unit>" live.
    let total = tasks.len();
    // gidx -> "<module> > <unit>" for the per-item progress line.
    let unit_label: HashMap<usize, String> = source_slots
        .iter()
        .map(|(g, m, u)| (*g, format!("{m} \u{203a} {u}")))
        .collect();
    progress(&format!("Fetching {total} units\u{2026}"));
    let fetches = stream::iter(tasks)
        .map(|t| async move {
            let r = match (t.slug.as_deref(), t.base.as_deref()) {
                (Some(slug), Some(base)) => {
                    let res = if t.is_kc {
                        assemble_quiz(fetcher, index, slug, base, &t.module_url).await
                    } else {
                        assemble_unit(
                            fetcher, index, slug, base, &t.module_url, &t.unit_uid, t.position,
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
        .buffer_unordered(FETCH_CONCURRENCY);
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
    Ok(bytes)
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

fn cover_body(book: &Book, date_stamp: &str) -> String {
    badge_img(&book.icon_url)
        + &format!(
        "<p class=\"muted\">Microsoft Learn study export, assembled {date} for personal study. \
         Content is Microsoft's; original sources are listed at the end.</p>\n\
         <p><strong>Certification:</strong> {title}</p>\n\
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

async fn assemble_unit<F: Fetcher>(
    fetcher: &F,
    index: &ContentIndex,
    slug: &str,
    folder_raw_base: &str,
    module_url: &str,
    unit_uid: &str,
    position: usize,
) -> Result<(String, String, String), ResolveError> {
    let unit_slug = unit_slug_from_uid(unit_uid);
    // Primary: match by uid slug. Fallback: the file numbered `position` (handles units
    // whose uid slug differs from the file slug).
    let file = index
        .unit_include_file(slug, unit_slug)
        .or_else(|| index.unit_include_file_by_number(slug, position));
    match file {
        Some(file) => {
            let url = format!("{folder_raw_base}/includes/{file}");
            let md = fetcher.get_json(&url).await?;
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
