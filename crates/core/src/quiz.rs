//! Knowledge-check quiz parsing. The unit YAML in `MicrosoftDocs/learn` carries a
//! `quiz:` block with questions, choices, correctness, and explanations - this is the
//! "exam examples for knowledge checking" source.

use serde::Deserialize;

/// Top of a `*-knowledge-check.yml` unit file; we only care about the `quiz` block and
/// ignore the rest (uid, metadata, content, ...).
#[derive(Debug, Deserialize)]
struct QuizFile {
    quiz: Option<Quiz>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Quiz {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub choices: Vec<Choice>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub content: String,
    #[serde(default, rename = "isCorrect")]
    pub is_correct: bool,
    #[serde(default)]
    pub explanation: String,
}

/// Parse a knowledge-check YAML file, returning its quiz if present.
pub fn parse_quiz(yaml: &str) -> Result<Option<Quiz>, serde_yaml::Error> {
    let f: QuizFile = serde_yaml::from_str(yaml)?;
    Ok(f.quiz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quiz_block() {
        let y = r#"### YamlMime:ModuleUnit
uid: learn.wwl.x.knowledge-check
title: Module assessment
content: |
  [!include[](includes/10-knowledge-check.md)]
quiz:
  title: ""
  questions:
  - content: "Q1?"
    choices:
    - content: "A"
      isCorrect: false
      explanation: "nope"
    - content: "B"
      isCorrect: true
      explanation: "yes"
"#;
        let q = parse_quiz(y).unwrap().unwrap();
        assert_eq!(q.questions.len(), 1);
        assert_eq!(q.questions[0].choices.len(), 2);
        assert!(q.questions[0].choices[1].is_correct);
        assert_eq!(q.questions[0].choices[1].explanation, "yes");
    }
}
