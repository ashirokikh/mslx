//! Bulk knowledge-check question export. Walks a certification's parts -> modules,
//! fetches each module's knowledge-check YAML from the public repo, and emits a flat,
//! serializable question bank tagged by domain (learning path). This is the data source
//! for downstream study tools: one self-contained JSON, no live Learn/GitHub calls at
//! study time.

use serde::Serialize;

use crate::quiz;
use crate::{module_slug_from_url, resolve_certification, ContentIndex, Fetcher, ResolveError};

/// A whole certification's knowledge-check questions, grouped by domain.
#[derive(Debug, Serialize)]
pub struct QuestionBank {
    pub cert_uid: String,
    pub title: String,
    /// Generation date stamp (YYYY-MM-DD), for the citation/freshness requirement.
    pub generated: String,
    pub domains: Vec<Domain>,
}

/// One learning path = one exam domain.
#[derive(Debug, Serialize)]
pub struct Domain {
    pub uid: String,
    pub title: String,
    pub questions: Vec<ExportQuestion>,
}

#[derive(Debug, Serialize)]
pub struct ExportQuestion {
    pub module_uid: String,
    pub module_title: String,
    pub content: String,
    /// True when more than one choice is correct (select-all-that-apply).
    pub multi: bool,
    pub choices: Vec<ExportChoice>,
}

#[derive(Debug, Serialize)]
pub struct ExportChoice {
    pub content: String,
    pub correct: bool,
    pub explanation: String,
}

/// Resolve the cert tree, then fetch + parse every module's knowledge-check quiz.
///
/// Sequential on purpose: a cert is ~20-30 modules, so the handful of requests finish in
/// seconds and the code stays simple. Modules with no knowledge check, a missing raw
/// base, or an unfetchable/unparseable YAML are skipped (graceful, not fatal) so one gap
/// never sinks the whole bank.
pub async fn build_question_bank<F: Fetcher>(
    fetcher: &F,
    index: &ContentIndex,
    input: &str,
    locale: &str,
    date_stamp: &str,
    progress: &dyn Fn(&str),
) -> Result<QuestionBank, ResolveError> {
    let book = resolve_certification(fetcher, input, locale).await?;

    let mut domains = Vec::new();
    for part in &book.parts {
        let mut questions = Vec::new();
        for module in &part.modules {
            let Some(slug) = module.url.as_deref().and_then(module_slug_from_url) else {
                continue;
            };
            let Some(base) = index.module_raw_base(&slug) else {
                continue;
            };
            let Some(kc_file) = index.knowledge_check_yml(&slug) else {
                continue;
            };
            let raw_url = format!("{base}/{kc_file}");
            let Ok(yaml) = fetcher.get_json(&raw_url).await else {
                continue;
            };
            let quiz = match quiz::parse_quiz(&yaml) {
                Ok(Some(q)) => q,
                _ => continue,
            };
            for q in quiz.questions {
                let correct_count = q.choices.iter().filter(|c| c.is_correct).count();
                questions.push(ExportQuestion {
                    module_uid: module.uid.clone(),
                    module_title: module.title.clone(),
                    content: q.content,
                    multi: correct_count > 1,
                    choices: q
                        .choices
                        .into_iter()
                        .map(|c| ExportChoice {
                            content: c.content,
                            correct: c.is_correct,
                            explanation: c.explanation,
                        })
                        .collect(),
                });
            }
        }
        progress(&format!(
            "  {}: {} questions",
            part.title,
            questions.len()
        ));
        domains.push(Domain {
            uid: part.uid.clone(),
            title: part.title.clone(),
            questions,
        });
    }

    Ok(QuestionBank {
        cert_uid: book.cert_uid,
        title: book.title,
        generated: date_stamp.to_string(),
        domains,
    })
}
