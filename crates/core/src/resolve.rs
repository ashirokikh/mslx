//! Certification -> exam-code overrides for the resolution chain.
//!
//! Microsoft's catalog API exposes a study guide for some certifications/exams but leaves
//! others empty: AZ-900, MB-800 and several fundamentals come back with no `exams` and no
//! `study_guide`, so the cert -> exam -> study-guide chain has nothing to follow.
//!
//! The authoritative path list for those exams lives on their instructor-led **course**
//! (`course.<code>t00`), which the engine resolves generically. The only thing it can't
//! derive is which exam a *certification uid* maps to (the catalog doesn't link them and the
//! cert page is JS-rendered), so we keep that small, stable mapping here. No learning-path
//! uids are hardcoded - they always come from the live course study guide.

/// Maps a certification uid to its exam code, for the certs whose catalog entry has neither a
/// study guide nor a linked exam. Returns `None` to fall through to normal resolution.
pub fn curated_exam_code(cert_uid: &str) -> Option<&'static str> {
    Some(match cert_uid.trim().to_lowercase().as_str() {
        "certification.azure-fundamentals" => "az-900",
        "certification.d365-business-central-functional-consultant-associate" => "mb-800",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_certs_to_exam_codes() {
        assert_eq!(curated_exam_code("certification.azure-fundamentals"), Some("az-900"));
        assert_eq!(
            curated_exam_code("certification.d365-business-central-functional-consultant-associate"),
            Some("mb-800"),
        );
    }

    #[test]
    fn unknown_falls_through() {
        // Certs that resolve normally (catalog study guide) must not be overridden.
        assert!(curated_exam_code("certification.azure-ai-fundamentals").is_none());
        assert!(curated_exam_code("certification.azure-solutions-architect").is_none());
    }
}
