//! mslx - export Microsoft Learn certifications, exams, and paths to EPUB study books.
//!
//! Usage:
//!   mslx <cert-url-or-uid> [--locale en-us] [--json]
//!
//! Examples:
//!   mslx https://learn.microsoft.com/en-us/credentials/certifications/azure-solutions-architect/
//!   mslx certification.azure-solutions-architect
//!   mslx azure-solutions-architect

use async_trait::async_trait;
use mslx_core::{
    fetch_unit_markdown, module_slug_from_url, render_outline, resolve_certification,
    unit_slug_from_uid, ContentIndex, FetchError, Fetcher,
};

struct ReqwestFetcher {
    client: reqwest::Client,
}

#[async_trait]
impl Fetcher for ReqwestFetcher {
    async fn get_json(&self, url: &str) -> Result<String, FetchError> {
        let mkerr = |m: String| FetchError {
            url: url.to_string(),
            message: m,
        };
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| mkerr(e.to_string()))?;
        let resp = resp.error_for_status().map_err(|e| mkerr(e.to_string()))?;
        resp.text().await.map_err(|e| mkerr(e.to_string()))
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        let mkerr = |m: String| FetchError {
            url: url.to_string(),
            message: m,
        };
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| mkerr(e.to_string()))?;
        let resp = resp.error_for_status().map_err(|e| mkerr(e.to_string()))?;
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| mkerr(e.to_string()))
    }

    async fn sleep_ms(&self, ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut pos: Vec<String> = Vec::new();
    let mut locale = "en-us".to_string();
    let mut as_json = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--locale" => locale = args.next().unwrap_or(locale),
            "--json" => as_json = true,
            "-h" | "--help" => {
                eprintln!(
                    "mslx <cert-url-or-uid> [--locale en-us] [--json]\n\
                     mslx unit <module-url-or-slug> <unit-uid>\n\
                     mslx epub <module-uid> [out.epub]\n\
                     mslx book <cert-url-or-uid> [out.epub]\n\
                     mslx questions <cert-url-or-uid> [out.json]\n\n\
                     resolve: Catalog API tree of a certification (paths -> modules -> units).\n\
                     unit:    fetch one unit's markdown from the public MicrosoftDocs/learn repo.\n\
                     epub:    assemble one module (units + knowledge check) into an EPUB.\n\
                     book:    assemble a whole certification into one EPUB (nested TOC)."
                );
                return Ok(());
            }
            other => pos.push(other.to_string()),
        }
    }

    let fetcher = ReqwestFetcher {
        client: reqwest::Client::builder()
            .user_agent("mslx/0.1")
            .build()?,
    };

    // Subcommand: `questions <cert-url-or-uid> [out.json]` - export the knowledge-check
    // question bank as domain-tagged JSON (the study-tool data source).
    if pos.first().map(String::as_str) == Some("questions") {
        let input = pos
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("questions: missing <cert-url-or-uid>"))?;
        let out = pos.get(2).cloned().unwrap_or_else(|| "questions.json".to_string());
        let index = load_index(&fetcher).await?;
        eprintln!("(indexed {} modules)", index.modules.len());
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let progress = |msg: &str| eprintln!("{msg}");
        let bank = mslx_core::export::build_question_bank(
            &fetcher, &index, input, &locale, &date, &progress,
        )
        .await?;
        let total: usize = bank.domains.iter().map(|d| d.questions.len()).sum();
        let json = serde_json::to_string_pretty(&bank)?;
        std::fs::write(&out, &json)?;
        eprintln!(
            "wrote {out} ({total} questions across {} domains)",
            bank.domains.len()
        );
        return Ok(());
    }

    // Subcommand: `book <cert-url-or-uid> [out.epub]` - whole certification.
    if pos.first().map(String::as_str) == Some("book") {
        let input = pos
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("book: missing <cert-url-or-uid>"))?;
        let out = pos.get(2).cloned().unwrap_or_else(|| "certification.epub".to_string());
        let index = load_index(&fetcher).await?;
        eprintln!("(indexed {} modules)", index.modules.len());
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let progress = |msg: &str| eprintln!("  {msg}");
        let bytes = mslx_core::assemble::build_certification_epub(
            &fetcher, &index, input, &locale, &date, &progress,
        )
        .await?;
        std::fs::write(&out, &bytes)?;
        eprintln!("wrote {out} ({} bytes)", bytes.len());
        return Ok(());
    }

    // Subcommand: `epub <module-uid> [out.epub]`
    if pos.first().map(String::as_str) == Some("epub") {
        let module_uid = pos
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("epub: missing <module-uid>"))?;
        let out = pos.get(2).cloned().unwrap_or_else(|| "module.epub".to_string());
        let index = load_index(&fetcher).await?;
        eprintln!("(indexed {} modules)", index.modules.len());
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let bytes =
            mslx_core::assemble::build_module_epub(&fetcher, &index, module_uid, &locale, &date)
                .await?;
        std::fs::write(&out, &bytes)?;
        eprintln!("wrote {out} ({} bytes)", bytes.len());
        return Ok(());
    }

    // Subcommand: `unit <module-url-or-slug> <unit-uid>`
    if pos.first().map(String::as_str) == Some("unit") {
        let module_arg = pos
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("unit: missing <module-url-or-slug>"))?;
        let unit_uid = pos
            .get(2)
            .ok_or_else(|| anyhow::anyhow!("unit: missing <unit-uid>"))?;
        let module_slug = if module_arg.contains('/') {
            module_slug_from_url(module_arg)
                .ok_or_else(|| anyhow::anyhow!("could not parse a module slug from {module_arg}"))?
        } else {
            module_arg.clone()
        };

        let index = load_index(&fetcher).await?;
        eprintln!("(indexed {} modules)", index.modules.len());
        if index.truncated {
            eprintln!("warning: git tree was truncated; some modules may be missing");
        }
        let url = index
            .unit_markdown_url(&module_slug, unit_slug_from_uid(unit_uid))
            .ok_or_else(|| {
                anyhow::anyhow!("no markdown for unit {unit_uid} in module '{module_slug}'")
            })?;
        eprintln!("source: {url}\n");
        let md = fetch_unit_markdown(&fetcher, &index, &module_slug, unit_uid).await?;
        print!("{md}");
        return Ok(());
    }

    // Default: resolve a certification into its tree.
    let input = pos
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing argument: a certification URL or uid. Try --help"))?;
    let book = resolve_certification(&fetcher, &input, &locale).await?;
    if as_json {
        // Lightweight JSON for piping into other tools.
        println!("{}", to_debug_json(&book));
    } else {
        print!("{}", render_outline(&book));
    }
    Ok(())
}

const TREE_API: &str = "https://api.github.com/repos/MicrosoftDocs/learn/git/trees";

/// Load a COMPLETE content index, caching the assembled index under the temp dir.
///
/// A single recursive tree of the whole repo exceeds GitHub's response cap and comes
/// back `truncated`, silently dropping modules. So we walk per group instead: root ->
/// `learn-pr` -> its 52 group subtrees (each well under the cap) -> union. Cached so the
/// ~54 requests happen once. (For the browser, ship this index prebuilt - see PLAN.)
async fn load_index(fetcher: &ReqwestFetcher) -> anyhow::Result<ContentIndex> {
    let cache = std::env::temp_dir().join("mslx-content-index.json");
    if let Ok(s) = std::fs::read_to_string(&cache) {
        if let Ok(idx) = serde_json::from_str::<ContentIndex>(&s) {
            eprintln!("(using cached index: {})", cache.display());
            return Ok(idx);
        }
    }

    eprintln!("(building content index from learn-pr group subtrees, one-time)...");
    // root -> learn-pr sha
    let root: serde_json::Value = serde_json::from_str(&fetcher.get_json(&format!("{TREE_API}/main")).await?)?;
    let lp_sha = subtree_sha(&root, "learn-pr")
        .ok_or_else(|| anyhow::anyhow!("learn-pr folder not found in repo root"))?;
    // learn-pr -> group (name, sha) pairs
    let lp: serde_json::Value =
        serde_json::from_str(&fetcher.get_json(&format!("{TREE_API}/{lp_sha}")).await?)?;
    let groups: Vec<(String, String)> = lp["tree"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|e| e["type"] == "tree")
                .filter_map(|e| {
                    Some((e["path"].as_str()?.to_string(), e["sha"].as_str()?.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut idx = ContentIndex::default();
    for (name, sha) in &groups {
        let gj = fetcher
            .get_json(&format!("{TREE_API}/{sha}?recursive=1"))
            .await?;
        idx.add_group_tree(name, &gj)?;
    }
    eprintln!("(indexed {} groups)", groups.len());

    if let Ok(s) = serde_json::to_string(&idx) {
        let _ = std::fs::write(&cache, s);
    }
    Ok(idx)
}

fn subtree_sha(tree: &serde_json::Value, path: &str) -> Option<String> {
    tree["tree"]
        .as_array()?
        .iter()
        .find(|e| e["path"] == path)
        .and_then(|e| e["sha"].as_str())
        .map(String::from)
}

// Minimal hand-rolled JSON of the resolved tree, for piping into other tools (the core
// types are not Serialize).
fn to_debug_json(book: &mslx_core::Book) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let mut o = String::new();
    o.push_str(&format!(
        "{{\"cert_uid\":\"{}\",\"title\":\"{}\",\"parts\":[",
        esc(&book.cert_uid),
        esc(&book.title)
    ));
    for (pi, p) in book.parts.iter().enumerate() {
        if pi > 0 {
            o.push(',');
        }
        o.push_str(&format!(
            "{{\"uid\":\"{}\",\"title\":\"{}\",\"modules\":[",
            esc(&p.uid),
            esc(&p.title)
        ));
        for (mi, m) in p.modules.iter().enumerate() {
            if mi > 0 {
                o.push(',');
            }
            o.push_str(&format!(
                "{{\"uid\":\"{}\",\"title\":\"{}\",\"units\":[",
                esc(&m.uid),
                esc(&m.title)
            ));
            for (ui, u) in m.units.iter().enumerate() {
                if ui > 0 {
                    o.push(',');
                }
                o.push_str(&format!(
                    "{{\"uid\":\"{}\",\"title\":\"{}\",\"kc\":{}}}",
                    esc(&u.uid),
                    esc(&u.title),
                    u.is_knowledge_check
                ));
            }
            o.push_str("]}");
        }
        o.push_str("]}");
    }
    o.push_str("]}");
    o
}
