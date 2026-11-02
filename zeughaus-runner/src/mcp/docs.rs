//! Plans and proposals as an agent and a script read them, and what a plan
//! file says about itself. Pure: the clock is an argument.

use zeughaus_link::{DocView, doc_groups};

/// `5m`, `3h`, `2d`: whole minutes below an hour, whole hours below a day,
/// else whole days.
pub fn age(seconds: u64) -> String {
    match seconds {
        0..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// One document with its body:
///
/// ```text
/// #4 plan saved: Store plans in the CI
/// from zeughaus
/// created 3h ago
///
/// <body>
/// ```
pub fn doc_text(doc: &DocView, now: u64) -> String {
    format!(
        "#{} {} {}: {}\nfrom {}\ncreated {} ago\n\n{}",
        doc.number,
        doc.kind.name(),
        doc.state.name(),
        doc.title,
        doc.source,
        age(now.saturating_sub(doc.created)),
        doc.shown_text()
    )
}

/// One line per document, under the heading of its group: awaiting
/// approval, the queue in the order a master takes it, in progress, open
/// proposals, then the history.
pub fn docs_text(docs: &[DocView], now: u64) -> String {
    let mut text = String::new();
    for (group, members) in doc_groups(docs) {
        text.push_str(&format!("{}:\n", group.title()));
        for doc in members {
            text.push_str(&format!(
                "  #{} {} {}: {} ({} ago)\n",
                doc.number,
                doc.kind.name(),
                doc.state.name(),
                doc.title,
                age(now.saturating_sub(doc.changed))
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use zeughaus_link::{DocKind, DocState};

    use super::*;

    fn doc(number: u64, kind: DocKind, state: DocState) -> DocView {
        DocView {
            repo: "demo".to_owned(),
            number,
            kind,
            title: format!("Doc {number}"),
            state,
            source: "weida".to_owned(),
            created: 1_000,
            changed: 1_000 + number,
            approved: None,
            proposals: Vec::new(),
            plan: None,
            runs_on: Vec::new(),
            placement: Vec::new(),
            body: String::new(),
            onboard: None,
            note: String::new(),
        }
    }

    #[test]
    fn ages_step_at_each_unit_boundary() {
        assert_eq!(age(0), "0m");
        assert_eq!(age(3599), "59m");
        assert_eq!(age(3600), "1h");
        assert_eq!(age(86_399), "23h");
        assert_eq!(age(86_400), "1d");
        assert_eq!(age(10 * 86_400), "10d");
    }

    #[test]
    fn a_document_reads_as_header_then_body() {
        let mut plan = doc(4, DocKind::Plan, DocState::Saved);
        plan.body = "# Doc 4\n\nDo it.\n".to_owned();
        assert_eq!(
            doc_text(&plan, 1_000 + 3 * 3600),
            "#4 plan saved: Doc 4\nfrom weida\ncreated 3h ago\n\n# Doc 4\n\nDo it.\n"
        );
    }

    #[test]
    fn the_list_puts_what_waits_for_approval_first() {
        let docs = [
            doc(1, DocKind::Plan, DocState::Done),
            doc(2, DocKind::Proposal, DocState::Open),
            doc(3, DocKind::Plan, DocState::Saved),
        ];
        assert_eq!(
            docs_text(&docs, 1_000 + 120),
            "Awaiting approval:\n  #3 plan saved: Doc 3 (1m ago)\n\
             Open proposals:\n  #2 proposal open: Doc 2 (1m ago)\n\
             History:\n  #1 plan done: Doc 1 (1m ago)\n"
        );
    }
}
