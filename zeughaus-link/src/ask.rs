//! An agent's open question (omp's `ask`), posted to the CI by the zeughaus
//! omp plugin and answered from an editor or from the agent's own terminal,
//! whichever is first.

use serde::{Deserialize, Serialize};

/// An ask not polled for this long is dropped.
pub const ASK_LEASE_SECS: u64 = 30;
pub const MAX_ASK_QUESTIONS: usize = 10;
pub const MAX_ASK_BYTES: usize = 32 << 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    pub id: String,
    pub question: String,
    pub options: Vec<AskOption>,
    #[serde(default)]
    pub multi: bool,
    /// Index of the recommended option.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

/// The answer to one question: the chosen labels and/or a typed answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskReply {
    pub id: String,
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskView {
    pub id: u64,
    pub holder: crate::ci::ClaimHolder,
    /// The repository and ticket the asking session works on; empty and
    /// `None` for a user's session outside one.
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub doc: Option<u64>,
    /// The holder's CI session as the editor names it (`plan zeughaus #49`),
    /// or empty.
    #[serde(default)]
    pub label: String,
    pub questions: Vec<AskQuestion>,
    /// Unix seconds.
    pub asked: u64,
    pub polled: u64,
    #[serde(default)]
    pub answer: Option<Vec<AskReply>>,
}

/// Whether `questions` can be posted as one ask.
pub fn check_questions(questions: &[AskQuestion]) -> Result<(), String> {
    if questions.is_empty() {
        return Err("an ask has at least one question".to_owned());
    }
    if questions.len() > MAX_ASK_QUESTIONS {
        return Err(format!("an ask has at most {MAX_ASK_QUESTIONS} questions"));
    }
    for (i, question) in questions.iter().enumerate() {
        if questions[..i].iter().any(|q| q.id == question.id) {
            return Err(format!("question ids must be unique: {}", question.id));
        }
    }
    for question in questions {
        for (i, option) in question.options.iter().enumerate() {
            if question.options[..i]
                .iter()
                .any(|o| o.label == option.label)
            {
                return Err(format!(
                    "option labels must be unique within a question: {}",
                    option.label
                ));
            }
        }
    }
    let size = serde_json::to_vec(questions).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_ASK_BYTES {
        return Err(format!("an ask is at most {MAX_ASK_BYTES} bytes"));
    }
    Ok(())
}

/// Whether `replies` answer `questions`, one reply per question in order. A
/// multi-select question may stay empty, a single-select one needs an option
/// or a typed answer.
pub fn check_replies(questions: &[AskQuestion], replies: &[AskReply]) -> Result<(), String> {
    if replies.len() != questions.len() {
        return Err(format!(
            "{} replies for {} questions",
            replies.len(),
            questions.len()
        ));
    }
    for (i, (question, reply)) in questions.iter().zip(replies).enumerate() {
        if reply.id != question.id {
            return Err(format!(
                "reply {i} is for {}, not {}",
                reply.id, question.id
            ));
        }
        if let Some(label) = reply
            .selected
            .iter()
            .find(|label| !question.options.iter().any(|o| &o.label == *label))
        {
            return Err(format!("\"{label}\" is no option of {}", question.id));
        }
        if !question.multi {
            if reply.selected.len() > 1 {
                return Err(format!("{} takes one option", question.id));
            }
            let typed = reply
                .custom
                .as_deref()
                .is_some_and(|c| !c.trim().is_empty());
            if reply.selected.is_empty() && !typed {
                return Err(format!("{} has no answer", question.id));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(id: &str, labels: &[&str], multi: bool) -> AskQuestion {
        AskQuestion {
            id: id.to_owned(),
            question: format!("{id}?"),
            options: labels
                .iter()
                .map(|label| AskOption {
                    label: (*label).to_owned(),
                    description: None,
                    preview: None,
                })
                .collect(),
            multi,
            recommended: None,
            header: None,
        }
    }

    fn reply(id: &str, selected: &[&str], custom: Option<&str>) -> AskReply {
        AskReply {
            id: id.to_owned(),
            selected: selected.iter().map(|s| (*s).to_owned()).collect(),
            custom: custom.map(str::to_owned),
        }
    }

    #[test]
    fn questions_are_refused_in_order() {
        assert_eq!(
            check_questions(&[]),
            Err("an ask has at least one question".to_owned())
        );
        let many: Vec<_> = (0..11)
            .map(|i| question(&format!("q{i}"), &["a"], false))
            .collect();
        assert_eq!(
            check_questions(&many),
            Err("an ask has at most 10 questions".to_owned())
        );
        assert_eq!(
            check_questions(&[question("a", &["x"], false), question("a", &["y"], false)]),
            Err("question ids must be unique: a".to_owned())
        );
        assert_eq!(
            check_questions(&[question("a", &["x", "y", "x"], false)]),
            Err("option labels must be unique within a question: x".to_owned())
        );
        let mut big = question("a", &["x"], false);
        big.question = "q".repeat(MAX_ASK_BYTES);
        assert_eq!(
            check_questions(&[big]),
            Err("an ask is at most 32768 bytes".to_owned())
        );
    }

    #[test]
    fn single_and_multi_asks_pass() {
        assert_eq!(
            check_questions(&[question("db", &["SQLite", "Postgres"], false)]),
            Ok(())
        );
        assert_eq!(
            check_questions(&[
                question("db", &["SQLite", "Postgres"], false),
                question("os", &["linux", "macos", "windows"], true),
            ]),
            Ok(())
        );
    }

    #[test]
    fn replies_are_refused_in_order() {
        let single = [question("db", &["SQLite", "Postgres"], false)];
        assert_eq!(
            check_replies(&single, &[]),
            Err("0 replies for 1 questions".to_owned())
        );
        assert_eq!(
            check_replies(&single, &[reply("os", &["SQLite"], None)]),
            Err("reply 0 is for os, not db".to_owned())
        );
        assert_eq!(
            check_replies(&single, &[reply("db", &["MySQL"], None)]),
            Err("\"MySQL\" is no option of db".to_owned())
        );
        assert_eq!(
            check_replies(&single, &[reply("db", &["SQLite", "Postgres"], None)]),
            Err("db takes one option".to_owned())
        );
        assert_eq!(
            check_replies(&single, &[reply("db", &[], Some("  "))]),
            Err("db has no answer".to_owned())
        );
    }

    #[test]
    fn empty_multi_and_custom_only_single_are_answers() {
        let questions = [
            question("db", &["SQLite", "Postgres"], false),
            question("os", &["linux", "macos"], true),
        ];
        assert_eq!(
            check_replies(
                &questions,
                &[reply("db", &[], Some("x")), reply("os", &[], None)]
            ),
            Ok(())
        );
    }
}
