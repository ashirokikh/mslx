//! Microsoft Learn exporter - IO-agnostic engine.
//!
//! This crate knows how to turn a certification input (URL or uid) into an ordered
//! book tree (certification -> learning paths -> modules -> units) by querying the
//! Microsoft Learn Catalog API. It does NOT perform IO itself: callers provide a
//! [`Fetcher`], so the same logic compiles to a native CLI (reqwest) and to wasm
//! (browser `fetch`).
//!
//! Spike scope: tree resolution only. Unit *content* (markdown, knowledge checks) is a
//! later milestone - the Catalog API does not expose it (see PLAN.md section 4).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod assemble;
pub mod epub;
pub mod export;
pub mod markdown;
pub mod quiz;
pub mod resolve;

/// Catalog API base. The Catalog API sends `access-control-allow-origin: *`, so this
/// same URL is reachable from the browser too.
pub const CATALOG_BASE: &str = "https://learn.microsoft.com/api/catalog/";

/// Max uids per request. The API accepts comma-separated uids; we chunk to keep URLs
/// well under typical server limits when a cert expands to ~150 units.
const UID_CHUNK: usize = 25;

// ---------------------------------------------------------------------------
// Fetcher seam (the only IO boundary)
// ---------------------------------------------------------------------------

/// One Catalog API query: a content type, a set of uids, and a locale.
#[derive(Debug, Clone)]
pub struct CatalogQuery {
    pub content_type: String,
    pub uids: Vec<String>,
    pub locale: String,
}

impl CatalogQuery {
    /// Build the full request URL with literal commas between uids (matches the API).
    pub fn url(&self) -> String {
        format!(
            "{CATALOG_BASE}?type={}&uid={}&locale={}",
            self.content_type,
            self.uids.join(","),
            self.locale
        )
    }
}

/// Performs the actual HTTP GET and returns the raw JSON body. Implemented natively
/// with reqwest (CLI) and in the browser with `fetch` (wasm). The futures must be `Send`
/// on native (the CLI runs on a multi-threaded runtime) but cannot be on wasm, where
/// `JsFuture` is `!Send` - so the bound is target-gated.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait Fetcher {
    async fn get_json(&self, url: &str) -> Result<String, FetchError>;
    /// Fetch raw bytes (for binary assets like images).
    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, FetchError>;
}

#[derive(Debug, Error)]
#[error("fetch failed for {url}: {message}")]
pub struct FetchError {
    pub url: String,
    pub message: String,
}

// ---------------------------------------------------------------------------
// Raw Catalog API shapes (only the fields we use)
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
struct CatalogResponse {
    #[serde(default)]
    certifications: Vec<RawCert>,
    #[serde(default)]
    exams: Vec<RawExam>,
    #[serde(default, rename = "learningPaths")]
    learning_paths: Vec<RawPath>,
    #[serde(default)]
    courses: Vec<RawCourse>,
    #[serde(default)]
    modules: Vec<RawModule>,
    #[serde(default)]
    units: Vec<RawUnit>,
}

/// An instructor-led course. Unlike many exams/certs, a course carries its `study_guide`
/// (the ordered learning paths) in the catalog - the authoritative path list for exams that
/// otherwise resolve to nothing (AZ-900, MB-800, ...).
#[derive(Debug, Deserialize)]
struct RawCourse {
    uid: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    levels: Vec<String>,
    #[serde(default)]
    study_guide: Vec<StudyItem>,
}

/// The Microsoft certification badge for a difficulty level. Catalog cert badges are just the
/// generic level badge (beginner -> fundamentals, intermediate -> associate, advanced ->
/// expert), so an exam with no linked cert can still show the same badge its cert page does
/// instead of the generic course icon.
fn cert_level_badge(levels: &[String]) -> Option<String> {
    let level = match levels.first().map(String::as_str) {
        Some("beginner") => "fundamentals",
        Some("intermediate") => "associate",
        Some("advanced") => "expert",
        _ => return None,
    };
    Some(format!(
        "https://learn.microsoft.com/en-us/media/learn/certification/badges/microsoft-certified-{level}-badge.svg"
    ))
}

#[derive(Debug, Deserialize)]
struct RawCert {
    uid: String,
    title: String,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    study_guide: Vec<StudyItem>,
    /// Linked exam uids (e.g. `exam.az-305`). Empty for some certs.
    #[serde(default)]
    exams: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawExam {
    uid: Option<String>,
    title: Option<String>,
    #[serde(default)]
    icon_url: Option<String>,
    #[serde(default)]
    study_guide: Vec<StudyItem>,
}

#[derive(Debug, Deserialize)]
struct StudyItem {
    uid: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize)]
struct RawPath {
    uid: String,
    title: String,
    url: Option<String>,
    #[serde(default)]
    icon_url: Option<String>,
    duration_in_minutes: Option<u32>,
    #[serde(default)]
    modules: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawModule {
    uid: String,
    title: String,
    url: Option<String>,
    duration_in_minutes: Option<u32>,
    #[serde(default)]
    units: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawUnit {
    uid: String,
    title: String,
    duration_in_minutes: Option<u32>,
}

// ---------------------------------------------------------------------------
// Resolved book tree (the spike's output)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Book {
    pub cert_uid: String,
    pub title: String,
    /// Certification badge image URL, if known.
    pub icon_url: Option<String>,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone)]
pub struct Part {
    pub uid: String,
    pub title: String,
    pub url: Option<String>,
    /// Learning-path achievement badge image URL, if known.
    pub icon_url: Option<String>,
    pub duration_in_minutes: Option<u32>,
    pub modules: Vec<ModuleNode>,
}

#[derive(Debug, Clone)]
pub struct ModuleNode {
    pub uid: String,
    pub title: String,
    pub url: Option<String>,
    pub duration_in_minutes: Option<u32>,
    pub units: Vec<UnitNode>,
}

#[derive(Debug, Clone)]
pub struct UnitNode {
    pub uid: String,
    pub title: String,
    pub duration_in_minutes: Option<u32>,
    pub is_knowledge_check: bool,
}

impl UnitNode {
    fn from_raw(r: RawUnit) -> Self {
        let is_kc = r.uid.ends_with(".knowledge-check")
            || r.uid.ends_with(".module-assessment")
            || r.title.to_lowercase().contains("knowledge check");
        UnitNode {
            uid: r.uid,
            title: r.title,
            duration_in_minutes: r.duration_in_minutes,
            is_knowledge_check: is_kc,
        }
    }
}

impl Book {
    pub fn total_minutes(&self) -> u32 {
        self.parts
            .iter()
            .flat_map(|p| &p.modules)
            .filter_map(|m| m.duration_in_minutes)
            .sum()
    }
    pub fn module_count(&self) -> usize {
        self.parts.iter().map(|p| p.modules.len()).sum()
    }
    pub fn unit_count(&self) -> usize {
        self.parts
            .iter()
            .flat_map(|p| &p.modules)
            .map(|m| m.units.len())
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Input parsing: URL or uid -> certification uid
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("could not parse JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("could not derive a certification uid from input: {0}")]
    BadInput(String),
    #[error("certification not found in catalog: {0}")]
    CertNotFound(String),
    #[error("module not found in catalog: {0}")]
    ModuleNotFound(String),
    #[error("{0} has no learning paths in its study guide")]
    NoPaths(String),
    /// The book resolved (structure, titles) but none of its modules have content in the
    /// public source repo - e.g. Dynamics 365 / Business Central and Power Platform content
    /// is authored in private repositories with no public mirror, so it cannot be exported.
    #[error("no exportable source content found for \"{0}\"")]
    ContentUnavailable(String),
}

/// A classified user input: a certification, an exam, or a single learning path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearnInput {
    Cert(String),
    Exam(String),
    /// A single learning path, identified by its URL slug (`/training/paths/<slug>/`).
    Path(String),
}

/// Classify a user input as a certification or an exam. Accepts page URLs, `certification.*`
/// / `exam.*` uids, bare cert slugs (`azure-solutions-architect`), and bare exam codes
/// (`az-104`).
pub fn parse_input(input: &str) -> Result<LearnInput, ResolveError> {
    let s = input.trim().trim_end_matches('/');
    if s.is_empty() {
        return Err(ResolveError::BadInput("empty".into()));
    }
    if s.starts_with("certification.") {
        return Ok(LearnInput::Cert(s.to_string()));
    }
    if s.starts_with("exam.") {
        return Ok(LearnInput::Exam(s.to_string()));
    }
    if s.contains("://") || s.contains("learn.microsoft.com") {
        let path = s.split('?').next().unwrap_or(s);
        let segs: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        // A single learning path: `/training/paths/<slug>/`.
        if let Some(pos) = segs.iter().position(|&p| p == "paths") {
            if let Some(slug) = segs.get(pos + 1) {
                return Ok(LearnInput::Path(slug.to_string()));
            }
        }
        // Check exams first: `/certifications/exams/<code>/`.
        if let Some(pos) = segs.iter().position(|&p| p == "exams") {
            if let Some(code) = segs.get(pos + 1) {
                return Ok(LearnInput::Exam(format!("exam.{code}")));
            }
        }
        if let Some(pos) = segs.iter().position(|&p| p == "certifications") {
            if let Some(slug) = segs.get(pos + 1) {
                if *slug != "exams" {
                    return Ok(LearnInput::Cert(format!("certification.{slug}")));
                }
            }
        }
        return Err(ResolveError::BadInput(format!(
            "{s} is not a recognised certification or exam URL"
        )));
    }
    // Bare token: an exam code (az-104, az104, AZ 500, ...) vs a cert slug.
    if let Some(code) = canonical_exam_code(s) {
        return Ok(LearnInput::Exam(format!("exam.{code}")));
    }
    Ok(LearnInput::Cert(format!("certification.{s}")))
}

/// Canonicalize a bare exam code written in any common form - `AZ-500`, `az500`, `Az 500` -
/// to the `az-500` shape. Returns None if it does not look like an exam code (2 letters
/// followed by 2 to 4 digits, ignoring case, spaces, and hyphens).
fn canonical_exam_code(s: &str) -> Option<String> {
    let t: String = s
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let split = t.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = t.split_at(split);
    if letters.len() == 2 && (2..=4).contains(&digits.len()) && digits.chars().all(|c| c.is_ascii_digit()) {
        Some(format!("{letters}-{digits}"))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tree resolution
// ---------------------------------------------------------------------------

/// Resolve a certification input into the full ordered book tree using ~4 batched
/// rounds of Catalog API calls (cert -> paths -> modules -> units).
pub async fn resolve_certification<F: Fetcher>(
    fetcher: &F,
    input: &str,
    locale: &str,
) -> Result<Book, ResolveError> {
    // 1. Resolve the study guide (cert study_guide, with fallbacks for empty ones).
    let sg = resolve_study_guide(fetcher, input, locale).await?;
    let path_uids = sg.path_uids;

    // 2. Learning paths (keep study-guide order).
    let paths = fetch_all(fetcher, "learningPaths", &path_uids, locale)
        .await?
        .learning_paths;
    let paths = order_by(paths, &path_uids, |p| &p.uid);

    // 3. Modules across all paths (unique, first-seen order).
    let module_uids = dedup_ordered(paths.iter().flat_map(|p| p.modules.iter().cloned()));
    let modules = fetch_all(fetcher, "modules", &module_uids, locale)
        .await?
        .modules;
    let module_idx = index_by(modules, |m| m.uid.clone());

    // 4. Units across all modules (unique, first-seen order).
    let unit_uids = dedup_ordered(
        module_idx
            .values()
            .flat_map(|m| m.units.iter().cloned())
            .collect::<Vec<_>>()
            .into_iter(),
    );
    let units = fetch_all(fetcher, "units", &unit_uids, locale).await?.units;
    // Non-consuming lookup: a unit (or whole module) can be shared by two paths, so we
    // clone per reference rather than remove on first use.
    let unit_idx = index_by(units, |u| u.uid.clone());

    // Assemble, preserving order at every level.
    let mut parts = Vec::new();
    for p in paths {
        let mut mods = Vec::new();
        for muid in &p.modules {
            let Some(m) = module_idx.get(muid) else {
                continue;
            };
            let units: Vec<UnitNode> = m
                .units
                .iter()
                .filter_map(|uuid| {
                    unit_idx
                        .get(uuid)
                        .cloned()
                        .map(UnitNode::from_raw)
                        .or_else(|| placeholder_unit(uuid))
                })
                .collect();
            mods.push(ModuleNode {
                uid: m.uid.clone(),
                title: m.title.clone(),
                url: clean_url(m.url.clone()),
                duration_in_minutes: m.duration_in_minutes,
                units,
            });
        }
        parts.push(Part {
            uid: p.uid,
            title: p.title,
            url: clean_url(p.url),
            icon_url: p.icon_url,
            duration_in_minutes: p.duration_in_minutes,
            modules: mods,
        });
    }

    Ok(Book {
        cert_uid: sg.identifier,
        title: sg.title,
        icon_url: sg.icon_url,
        parts,
    })
}

/// Outcome of study-guide resolution: the book title/identifier plus the ordered learning
/// path uids that make up the study guide.
struct StudyGuide {
    title: String,
    identifier: String,
    icon_url: Option<String>,
    path_uids: Vec<String>,
}

/// Resolve any input to a study guide, applying fallbacks for study-guide-less certs:
/// cert.study_guide -> the cert's linked exam -> exam.study_guide -> learning-path title prefix.
async fn resolve_study_guide<F: Fetcher>(
    fetcher: &F,
    input: &str,
    locale: &str,
) -> Result<StudyGuide, ResolveError> {
    match parse_input(input)? {
        LearnInput::Cert(cert_uid) => {
            let resp = fetch_chunk(fetcher, "certifications", &[cert_uid.clone()], locale).await?;
            let cert = resp
                .certifications
                .into_iter()
                .find(|c| c.uid == cert_uid)
                .ok_or_else(|| ResolveError::CertNotFound(cert_uid.clone()))?;
            let (cert_title, cert_id, cert_icon, cert_exams, study) = (
                cert.title,
                cert.uid,
                cert.icon_url,
                cert.exams,
                cert.study_guide,
            );

            let paths = study_guide_paths(&study);
            if !paths.is_empty() {
                return Ok(StudyGuide {
                    title: cert_title,
                    identifier: cert_id,
                    icon_url: cert_icon,
                    path_uids: paths,
                });
            }
            // No catalog study guide. For certs we know map to an exam whose catalog entry is
            // also empty (AZ-900, MB-800), resolve the authoritative paths via the exam's
            // course. This works in the browser too (no cert-page scrape needed).
            if let Some(code) = resolve::curated_exam_code(&cert_id) {
                // Cert already supplies a proper title + badge, so ignore the course's.
                let (paths, _, _) = course_study_guide(fetcher, code, locale).await;
                if !paths.is_empty() {
                    return Ok(StudyGuide {
                        title: cert_title,
                        identifier: cert_id,
                        icon_url: cert_icon,
                        path_uids: paths,
                    });
                }
            }
            // Otherwise find the exam (linked, or discovered from the rendered cert page) and
            // build content from it, keeping the cert's title + badge.
            let exam_uid = match cert_exams.first() {
                Some(e) => Some(e.clone()),
                None => discover_exam_from_cert_page(fetcher, &cert_id, locale).await,
            };
            if let Some(exam_uid) = exam_uid {
                let exam = resolve_exam(fetcher, &exam_uid, locale).await?;
                if !exam.path_uids.is_empty() {
                    return Ok(StudyGuide {
                        title: cert_title,
                        identifier: cert_id,
                        icon_url: cert_icon,
                        path_uids: exam.path_uids,
                    });
                }
            }
            Err(ResolveError::NoPaths(format!(
                "{cert_id} (no study guide and could not resolve an exam; pass the exam code, e.g. az-104)"
            )))
        }
        LearnInput::Exam(exam_uid) => {
            let exam = resolve_exam(fetcher, &exam_uid, locale).await?;
            if exam.path_uids.is_empty() {
                return Err(ResolveError::NoPaths(exam_uid));
            }
            Ok(exam)
        }
        LearnInput::Path(slug) => {
            // A single learning path: find it by its URL slug. Its own title + achievement
            // badge become the book's cover.
            let resp = fetch_chunk(fetcher, "learningPaths", &[], locale).await?;
            let lp = resp
                .learning_paths
                .into_iter()
                .find(|p| {
                    p.url.as_deref().is_some_and(|u| {
                        u.split(['?', '#'])
                            .next()
                            .unwrap_or(u)
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            == Some(slug.as_str())
                    })
                })
                .ok_or_else(|| ResolveError::NoPaths(format!("learning path \"{slug}\"")))?;
            Ok(StudyGuide {
                title: lp.title,
                identifier: lp.uid.clone(),
                icon_url: lp.icon_url,
                path_uids: vec![lp.uid],
            })
        }
    }
}

/// Find the certification that owns `exam_uid` and return its badge icon, if any. Lets
/// exam-code exports (e.g. `mslx book az-305`) still show the cert's (Expert) badge rather
/// than dropping it - the cert is the real "thing that unites the book".
async fn cert_icon_for_exam<F: Fetcher>(
    fetcher: &F,
    exam_uid: &str,
    locale: &str,
) -> Option<String> {
    // Empty uid filter lists every certification; find the one linking this exam.
    let resp = fetch_chunk(fetcher, "certifications", &[], locale).await.ok()?;
    resp.certifications
        .into_iter()
        .filter(|c| c.exams.iter().any(|e| e == exam_uid))
        .find_map(|c| c.icon_url)
}

/// Resolve an exam to a study guide: prefer its own study_guide, else match learning paths
/// by the exam-code title prefix (e.g. `AZ-104:`).
async fn resolve_exam<F: Fetcher>(
    fetcher: &F,
    exam_uid: &str,
    locale: &str,
) -> Result<StudyGuide, ResolveError> {
    let resp = fetch_chunk(fetcher, "exams", &[exam_uid.to_string()], locale).await?;
    let exam = resp.exams.into_iter().find(|e| e.uid.as_deref() == Some(exam_uid));

    // Badge: prefer the owning cert's (e.g. Expert) badge; fall back to the exam's own.
    // Either way a badge is present whenever the catalog has one for this book.
    let exam_icon = exam.as_ref().and_then(|e| e.icon_url.clone());
    let icon_url = match cert_icon_for_exam(fetcher, exam_uid, locale).await {
        Some(i) => Some(i),
        None => exam_icon,
    };

    if let Some(exam) = &exam {
        let paths = study_guide_paths(&exam.study_guide);
        if !paths.is_empty() {
            let title = exam.title.clone().unwrap_or_else(|| exam_uid.to_string());
            return Ok(StudyGuide {
                title,
                identifier: exam_uid.to_string(),
                icon_url,
                path_uids: paths,
            });
        }
    }

    // Course-based resolution: an exam's instructor-led course carries its study guide in the
    // catalog even when the exam/cert do not (AZ-900, MB-800, DP-900, ...). This is the
    // authoritative path list and needs no per-exam curation.
    let (course_paths, course_title, course_icon) = course_study_guide(fetcher, exam_uid, locale).await;
    if !course_paths.is_empty() {
        // Fall back to the course's title + icon when the exam's own catalog entry is blank
        // (e.g. AZ-500), so the cover still gets a real name and logo instead of "exam.az-500".
        let title = exam
            .as_ref()
            .and_then(|e| e.title.clone())
            .or(course_title)
            .unwrap_or_else(|| exam_uid.to_string());
        return Ok(StudyGuide {
            title,
            identifier: exam_uid.to_string(),
            icon_url: icon_url.clone().or(course_icon),
            path_uids: course_paths,
        });
    }

    // Fallback 2: title-prefix match against all learning paths.
    let code = exam_code_label(exam_uid);
    let paths = learning_paths_by_prefix(fetcher, &code, locale).await?;
    // Prefer the exam's title; for a single matched path use that path's name; else generic.
    let title = exam
        .and_then(|e| e.title)
        .or_else(|| (paths.len() == 1).then(|| paths[0].1.clone()))
        .unwrap_or_else(|| format!("{code} study guide"));
    let path_uids = paths.into_iter().map(|(uid, _)| uid).collect();
    Ok(StudyGuide {
        title,
        identifier: exam_uid.to_string(),
        icon_url,
        path_uids,
    })
}

/// Resolve an exam's learning paths via its instructor-led course. The course numbering is
/// `<exam-code>T<NN>`; `T00` is the full official course, so `course.<code>t00` (with `t01`
/// as a fallback) holds the authoritative, ordered study guide for exams whose own catalog
/// entry has none.
/// Returns `(path_uids, course_title, course_icon)`. The title + icon let an exam whose own
/// catalog entry is blank (e.g. AZ-500) still get a real cover name and logo from its course.
async fn course_study_guide<F: Fetcher>(
    fetcher: &F,
    exam_uid: &str,
    locale: &str,
) -> (Vec<String>, Option<String>, Option<String>) {
    let code = exam_uid.strip_prefix("exam.").unwrap_or(exam_uid).to_lowercase();
    for suffix in ["t00", "t01"] {
        let uid = format!("course.{code}{suffix}");
        if let Ok(resp) = fetch_chunk(fetcher, "courses", &[uid.clone()], locale).await {
            if let Some(course) = resp.courses.into_iter().find(|c| c.uid == uid) {
                let paths = study_guide_paths(&course.study_guide);
                if !paths.is_empty() {
                    // Prefer the certification level badge (what the cert page shows) over the
                    // generic course icon.
                    let badge = cert_level_badge(&course.levels).or(course.icon_url);
                    return (paths, course.title, badge);
                }
            }
        }
    }
    (Vec::new(), None, None)
}

/// Discover a cert's exam by reading its rendered page, for certs whose catalog `exams`
/// field is empty (a data gap). The page references the exam (e.g. `exam.AZ-700`) even when
/// the API does not. CLI-only in practice; the browser would need a proxy for this page.
async fn discover_exam_from_cert_page<F: Fetcher>(
    fetcher: &F,
    cert_uid: &str,
    locale: &str,
) -> Option<String> {
    let slug = cert_uid.strip_prefix("certification.")?;
    let url = format!("https://learn.microsoft.com/{locale}/credentials/certifications/{slug}/");
    let html = fetcher.get_json(&url).await.ok()?;
    extract_exam_uid(&html)
}

/// First `exam.<code>` whose code looks like an exam (e.g. `az-700`), lowercased.
fn extract_exam_uid(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let mut from = 0;
    while let Some(rel) = lower[from..].find("exam.") {
        let start = from + rel + "exam.".len();
        let code: String = lower[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if canonical_exam_code(&code).is_some() {
            return Some(format!("exam.{code}"));
        }
        from = start;
    }
    None
}

fn study_guide_paths(items: &[StudyItem]) -> Vec<String> {
    items
        .iter()
        .filter(|i| i.kind == "learningPath")
        .map(|i| i.uid.clone())
        .collect()
}

/// `exam.az-104` -> `AZ-104`.
fn exam_code_label(exam_uid: &str) -> String {
    exam_uid
        .strip_prefix("exam.")
        .unwrap_or(exam_uid)
        .to_uppercase()
}

/// Find learning paths whose title starts with `<CODE>:` (e.g. `AZ-104:`). Fetches the full
/// learningPaths catalog once and filters; prerequisites paths float to the front.
async fn learning_paths_by_prefix<F: Fetcher>(
    fetcher: &F,
    code: &str,
    locale: &str,
) -> Result<Vec<(String, String)>, ResolveError> {
    let url = format!("{CATALOG_BASE}?type=learningPaths&locale={locale}");
    let body = fetcher.get_json(&url).await?;
    let resp: CatalogResponse = serde_json::from_str(&body)?;
    // Titles use either `AZ-104: ...` (colon) or `AZ-700 ...` (space) after the code.
    let code_upper = code.to_uppercase();
    let mut matched: Vec<RawPath> = resp
        .learning_paths
        .into_iter()
        .filter(|p| {
            p.title
                .to_uppercase()
                .strip_prefix(&code_upper)
                .is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with(':') || rest.starts_with(' ')
                })
        })
        .collect();
    // Stable sort: prerequisites first, otherwise keep catalog order.
    matched.sort_by_key(|p| !p.title.to_lowercase().contains("prerequisite"));
    Ok(matched.into_iter().map(|p| (p.uid, p.title)).collect())
}

/// Resolve a single module (by uid) into a `ModuleNode` with ordered, titled units.
pub async fn resolve_module<F: Fetcher>(
    fetcher: &F,
    module_uid: &str,
    locale: &str,
) -> Result<ModuleNode, ResolveError> {
    let resp = fetch_chunk(fetcher, "modules", &[module_uid.to_string()], locale).await?;
    let m = resp
        .modules
        .into_iter()
        .find(|m| m.uid == module_uid)
        .ok_or_else(|| ResolveError::ModuleNotFound(module_uid.to_string()))?;

    let units_resp = fetch_all(fetcher, "units", &m.units, locale).await?;
    let unit_idx = index_by(units_resp.units, |u| u.uid.clone());
    let units: Vec<UnitNode> = m
        .units
        .iter()
        .filter_map(|uuid| {
            unit_idx
                .get(uuid)
                .cloned()
                .map(UnitNode::from_raw)
                .or_else(|| placeholder_unit(uuid))
        })
        .collect();

    Ok(ModuleNode {
        uid: m.uid,
        title: m.title,
        url: clean_url(m.url),
        duration_in_minutes: m.duration_in_minutes,
        units,
    })
}

// A unit referenced by a module but missing from the units response (rare). Keep the
// uid visible rather than silently dropping it.
fn placeholder_unit(uuid: &str) -> Option<UnitNode> {
    // Derive a readable title from the uid's last segment (the unit slug) when the catalog
    // has no metadata for it, e.g. "...define-concepts-of-siem,-soar,-xdr" becomes
    // "Define concepts of siem, soar, xdr".
    let slug = uuid.rsplit('.').next().unwrap_or(uuid).replace('-', " ");
    let mut chars = slug.chars();
    let title = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => uuid.to_string(),
    };
    Some(UnitNode {
        uid: uuid.to_string(),
        title,
        duration_in_minutes: None,
        is_knowledge_check: uuid.ends_with(".knowledge-check"),
    })
}

// Strip the `?WT.mc_id=api_CatalogApi` tracking suffix the API appends to urls.
fn clean_url(url: Option<String>) -> Option<String> {
    url.map(|u| u.split('?').next().unwrap_or(&u).to_string())
}

// ---------------------------------------------------------------------------
// Fetch helpers (chunking + parsing live here so callers only do raw HTTP)
// ---------------------------------------------------------------------------

async fn fetch_chunk<F: Fetcher>(
    fetcher: &F,
    content_type: &str,
    uids: &[String],
    locale: &str,
) -> Result<CatalogResponse, ResolveError> {
    let q = CatalogQuery {
        content_type: content_type.to_string(),
        uids: uids.to_vec(),
        locale: locale.to_string(),
    };
    let body = fetcher.get_json(&q.url()).await?;
    Ok(serde_json::from_str(&body)?)
}

/// Fetch all uids for one content type, chunked, merged into a single response.
async fn fetch_all<F: Fetcher>(
    fetcher: &F,
    content_type: &str,
    uids: &[String],
    locale: &str,
) -> Result<CatalogResponse, ResolveError> {
    let mut acc = CatalogResponse::default();
    // The catalog uses commas to delimit the uid list, so a uid that itself contains a comma
    // (a rare Microsoft data quirk, e.g. an SC-900 unit "...siem,-soar,-xdr") can't be
    // batch-fetched and would 400 the whole request. Drop those here; callers already fall
    // back to a placeholder for any item the catalog does not return.
    let fetchable: Vec<String> = uids.iter().filter(|u| !u.contains(',')).cloned().collect();
    for chunk in fetchable.chunks(UID_CHUNK) {
        let r = fetch_chunk(fetcher, content_type, chunk, locale).await?;
        acc.certifications.extend(r.certifications);
        acc.exams.extend(r.exams);
        acc.learning_paths.extend(r.learning_paths);
        acc.modules.extend(r.modules);
        acc.units.extend(r.units);
    }
    Ok(acc)
}

// ---------------------------------------------------------------------------
// Small ordering helpers
// ---------------------------------------------------------------------------

fn dedup_ordered<I: Iterator<Item = String>>(it: I) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for u in it {
        if seen.insert(u.clone()) {
            out.push(u);
        }
    }
    out
}

fn index_by<T, K, F>(items: Vec<T>, key: F) -> std::collections::HashMap<K, T>
where
    K: std::hash::Hash + Eq,
    F: Fn(&T) -> K,
{
    items.into_iter().map(|t| (key(&t), t)).collect()
}

fn order_by<T, F>(items: Vec<T>, order: &[String], key: F) -> Vec<T>
where
    F: Fn(&T) -> &String,
{
    let mut map = std::collections::HashMap::new();
    for it in items {
        map.insert(key(&it).clone(), it);
    }
    order.iter().filter_map(|u| map.remove(u)).collect()
}

// ---------------------------------------------------------------------------
// Text rendering (reused by CLI and later the wasm UI's "preview")
// ---------------------------------------------------------------------------

/// Render the resolved tree as an indented outline with durations and totals.
pub fn render_outline(book: &Book) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}  [{}]\n", book.title, book.cert_uid));
    out.push_str(&format!(
        "{} parts, {} modules, {} units, ~{} min (~{:.1} h)\n\n",
        book.parts.len(),
        book.module_count(),
        book.unit_count(),
        book.total_minutes(),
        book.total_minutes() as f64 / 60.0
    ));
    for (pi, p) in book.parts.iter().enumerate() {
        out.push_str(&format!(
            "{}. {} ({} min)\n",
            pi + 1,
            p.title,
            p.duration_in_minutes.unwrap_or(0)
        ));
        for m in &p.modules {
            out.push_str(&format!(
                "    - {} ({} min)\n",
                m.title,
                m.duration_in_minutes.unwrap_or(0)
            ));
            for u in &m.units {
                let tag = if u.is_knowledge_check { "  [KC]" } else { "" };
                out.push_str(&format!(
                    "        . {}{} ({} min)\n",
                    u.title,
                    tag,
                    u.duration_in_minutes.unwrap_or(0)
                ));
            }
        }
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// Content index: map a module slug -> its files in MicrosoftDocs/learn, so a unit
// uid resolves to a raw-markdown URL. This is the "hard part" from PLAN section 4.
// ---------------------------------------------------------------------------

/// Recursive git tree of the public content repo (one fetch, cacheable).
pub const LEARN_TREE_URL: &str =
    "https://api.github.com/repos/MicrosoftDocs/learn/git/trees/main?recursive=1";
/// Raw content base for that repo's `main` branch (sends `access-control-allow-origin: *`).
pub const LEARN_RAW_BASE: &str = "https://raw.githubusercontent.com/MicrosoftDocs/learn/main";

#[derive(Debug, Deserialize)]
struct GitTree {
    tree: Vec<GitEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct GitEntry {
    path: String,
    #[serde(rename = "type")]
    kind: String,
}

/// Files belonging to one module folder under `learn-pr/<group>/<module-slug>/`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ModuleFiles {
    /// e.g. `learn-pr/wwl-azure/design-governance`
    pub folder: String,
    /// include markdown file names, e.g. `7-design-for-azure-policy.md`
    pub includes: Vec<String>,
    /// root yaml unit files, e.g. `10-knowledge-check.yml`
    pub units_yml: Vec<String>,
}

/// Index from module slug (the catalog module URL slug) to its repo files.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ContentIndex {
    pub modules: std::collections::HashMap<String, ModuleFiles>,
    /// GitHub truncates very large trees; if set, some modules may be missing.
    pub truncated: bool,
}

/// Build the content index from the git-tree JSON.
pub fn build_content_index(tree_json: &str) -> Result<ContentIndex, serde_json::Error> {
    let tree: GitTree = serde_json::from_str(tree_json)?;
    let mut modules: std::collections::HashMap<String, ModuleFiles> =
        std::collections::HashMap::new();

    for e in &tree.tree {
        if e.kind != "blob" {
            continue;
        }
        let segs: Vec<&str> = e.path.split('/').collect();
        // learn-pr / <group> / <module-slug> / <rest...>
        if segs.len() < 4 || segs[0] != "learn-pr" {
            continue;
        }
        let module_slug = segs[2].to_string();
        let folder = segs[..3].join("/");
        let entry = modules.entry(module_slug).or_insert_with(|| ModuleFiles {
            folder,
            ..Default::default()
        });
        let rest = &segs[3..];
        if rest.len() == 2 && rest[0] == "includes" && rest[1].ends_with(".md") {
            entry.includes.push(rest[1].to_string());
        } else if rest.len() == 1 && rest[0].ends_with(".yml") {
            entry.units_yml.push(rest[0].to_string());
        }
    }

    Ok(ContentIndex {
        modules,
        truncated: tree.truncated,
    })
}

impl ContentIndex {
    /// Absorb a single group subtree fetched via the git Trees API by its sha. Those paths
    /// are relative to `learn-pr/<group>` (e.g. `design-governance/includes/7-x.md`), so the
    /// group name is supplied to rebuild the full folder path. Call once per group; the union
    /// of all 52 groups is a complete, untruncated index (each group is under GitHub's cap).
    pub fn add_group_tree(
        &mut self,
        group: &str,
        tree_json: &str,
    ) -> Result<(), serde_json::Error> {
        let tree: GitTree = serde_json::from_str(tree_json)?;
        if tree.truncated {
            self.truncated = true;
        }
        for e in &tree.tree {
            if e.kind != "blob" {
                continue;
            }
            let segs: Vec<&str> = e.path.split('/').collect();
            // <module-slug> / <rest...>
            if segs.len() < 2 {
                continue;
            }
            let module_slug = segs[0].to_string();
            let folder = format!("learn-pr/{group}/{}", segs[0]);
            let entry = self.modules.entry(module_slug).or_insert_with(|| ModuleFiles {
                folder,
                ..Default::default()
            });
            let rest = &segs[1..];
            if rest.len() == 2 && rest[0] == "includes" && rest[1].ends_with(".md") {
                entry.includes.push(rest[1].to_string());
            } else if rest.len() == 1 && rest[0].ends_with(".yml") {
                entry.units_yml.push(rest[0].to_string());
            }
        }
        Ok(())
    }

    /// The include markdown file name for a unit (number-prefixed, e.g.
    /// `7-design-for-azure-policy.md`), matched by slug suffix.
    pub fn unit_include_file(&self, module_slug: &str, unit_slug: &str) -> Option<&str> {
        let m = self.modules.get(module_slug)?;
        let exact = format!("{unit_slug}.md");
        let suffixed = format!("-{unit_slug}.md");
        m.includes
            .iter()
            .find(|f| **f == exact || f.ends_with(&suffixed))
            .map(String::as_str)
    }

    /// Include file whose numeric prefix is `n` (e.g. n=9 -> `9-...md`). Some units have a
    /// uid slug that differs from the file slug (e.g. `design-for-azure-landing-zones` vs
    /// `9-design-for-landing-zones`); the catalog unit order matches the file numbering, so
    /// the 1-based position recovers those.
    pub fn unit_include_file_by_number(&self, module_slug: &str, n: usize) -> Option<&str> {
        let m = self.modules.get(module_slug)?;
        let prefix = format!("{n}-");
        m.includes
            .iter()
            .find(|f| f.starts_with(&prefix))
            .map(String::as_str)
    }

    /// Resolve a unit's prose markdown raw URL.
    pub fn unit_markdown_url(&self, module_slug: &str, unit_slug: &str) -> Option<String> {
        let m = self.modules.get(module_slug)?;
        let file = self.unit_include_file(module_slug, unit_slug)?;
        Some(format!("{LEARN_RAW_BASE}/{}/includes/{}", m.folder, file))
    }

    /// Absolute raw URL of the module folder (base for `media/...` and the KC yaml).
    pub fn module_raw_base(&self, module_slug: &str) -> Option<String> {
        self.modules
            .get(module_slug)
            .map(|m| format!("{LEARN_RAW_BASE}/{}", m.folder))
    }

    /// The knowledge-check yaml file name for a module, if any.
    pub fn knowledge_check_yml(&self, module_slug: &str) -> Option<&str> {
        self.modules
            .get(module_slug)?
            .units_yml
            .iter()
            .find(|f| f.ends_with("knowledge-check.yml"))
            .map(String::as_str)
    }
}

/// The module slug from a catalog module URL (`.../training/modules/<slug>/...`).
pub fn module_slug_from_url(url: &str) -> Option<String> {
    let path = url.split('?').next().unwrap_or(url);
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let pos = segs.iter().position(|&s| s == "modules")?;
    segs.get(pos + 1).map(|s| s.to_string())
}

/// The unit slug (last dotted segment) of a unit uid.
pub fn unit_slug_from_uid(uid: &str) -> &str {
    uid.rsplit('.').next().unwrap_or(uid)
}

/// Load and parse the content index via a [`Fetcher`].
pub async fn load_content_index<F: Fetcher>(fetcher: &F) -> Result<ContentIndex, ResolveError> {
    let json = fetcher.get_json(LEARN_TREE_URL).await?;
    Ok(build_content_index(&json)?)
}

/// Resolve a unit uid to its markdown and fetch it.
pub async fn fetch_unit_markdown<F: Fetcher>(
    fetcher: &F,
    index: &ContentIndex,
    module_slug: &str,
    unit_uid: &str,
) -> Result<String, ResolveError> {
    let slug = unit_slug_from_uid(unit_uid);
    let url = index.unit_markdown_url(module_slug, slug).ok_or_else(|| {
        ResolveError::BadInput(format!(
            "no markdown found for unit {unit_uid} in module '{module_slug}'"
        ))
    })?;
    Ok(fetcher.get_json(&url).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cert_url() {
        assert_eq!(
            parse_input(
                "https://learn.microsoft.com/en-us/credentials/certifications/azure-solutions-architect/"
            )
            .unwrap(),
            LearnInput::Cert("certification.azure-solutions-architect".into())
        );
    }

    #[test]
    fn passes_through_uids() {
        assert_eq!(
            parse_input("certification.azure-solutions-architect").unwrap(),
            LearnInput::Cert("certification.azure-solutions-architect".into())
        );
        assert_eq!(
            parse_input("exam.az-305").unwrap(),
            LearnInput::Exam("exam.az-305".into())
        );
    }

    #[test]
    fn bare_slug_vs_exam_code() {
        assert_eq!(
            parse_input("azure-solutions-architect").unwrap(),
            LearnInput::Cert("certification.azure-solutions-architect".into())
        );
        assert_eq!(
            parse_input("az-104").unwrap(),
            LearnInput::Exam("exam.az-104".into())
        );
    }

    #[test]
    fn exam_url_classified_as_exam() {
        assert_eq!(
            parse_input(
                "https://learn.microsoft.com/en-us/credentials/certifications/exams/az-305/"
            )
            .unwrap(),
            LearnInput::Exam("exam.az-305".into())
        );
    }

    #[test]
    fn exam_code_label_uppercases() {
        assert_eq!(exam_code_label("exam.az-104"), "AZ-104");
    }

    #[test]
    fn extracts_exam_uid_from_page() {
        let html = r#"<div>Required exams: <a href="/exams/az-700/">exam.AZ-700</a></div>"#;
        assert_eq!(extract_exam_uid(html), Some("exam.az-700".into()));
        // ignores non-exam "exam." text, finds the real code
        let html2 = "blah exam.foo then exam.sc-300 here";
        assert_eq!(extract_exam_uid(html2), Some("exam.sc-300".into()));
    }

    #[test]
    fn module_slug_and_unit_slug() {
        assert_eq!(
            module_slug_from_url(
                "https://learn.microsoft.com/en-us/training/modules/design-governance/?WT.mc_id=api"
            )
            .unwrap(),
            "design-governance"
        );
        assert_eq!(
            unit_slug_from_uid("learn.wwl.design-governance.design-for-azure-policy"),
            "design-for-azure-policy"
        );
    }

    #[test]
    fn index_resolves_unit_markdown_url() {
        let tree = r#"{
            "truncated": false,
            "tree": [
                {"path":"learn-pr/wwl-azure/design-governance","type":"tree"},
                {"path":"learn-pr/wwl-azure/design-governance/7-design-for-azure-policy.yml","type":"blob"},
                {"path":"learn-pr/wwl-azure/design-governance/10-knowledge-check.yml","type":"blob"},
                {"path":"learn-pr/wwl-azure/design-governance/includes/7-design-for-azure-policy.md","type":"blob"},
                {"path":"learn-pr/wwl-azure/design-governance/includes/1-introduction.md","type":"blob"}
            ]
        }"#;
        let idx = build_content_index(tree).unwrap();
        let m = idx.modules.get("design-governance").unwrap();
        assert_eq!(m.folder, "learn-pr/wwl-azure/design-governance");
        assert_eq!(m.includes.len(), 2);
        assert_eq!(m.units_yml.len(), 2);
        assert_eq!(
            idx.unit_markdown_url("design-governance", "design-for-azure-policy")
                .unwrap(),
            "https://raw.githubusercontent.com/MicrosoftDocs/learn/main/learn-pr/wwl-azure/design-governance/includes/7-design-for-azure-policy.md"
        );
        assert_eq!(
            idx.unit_markdown_url("design-governance", "introduction").unwrap(),
            "https://raw.githubusercontent.com/MicrosoftDocs/learn/main/learn-pr/wwl-azure/design-governance/includes/1-introduction.md"
        );
        assert!(idx.unit_markdown_url("design-governance", "nope").is_none());
    }
}
