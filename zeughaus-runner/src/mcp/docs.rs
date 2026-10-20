//! Plans and proposals as an agent and a script read them, and what a plan
//! file says about itself. Pure: the clock is an argument.

use zeughaus_link::{DocView, MAX_DOC_TITLE_BYTES, doc_groups};

/// How many lines under its title a plan may name its proposals in.
const PROPOSALS_WITHIN: usize = 20;

/// The title of a plan file: its first `# ` heading, cut to
/// [`MAX_DOC_TITLE_BYTES`] on a character boundary.
pub fn plan_title(body: &str) -> String {
    let Some(title) = body
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .map(str::trim)
        .filter(|title| !title.is_empty())
    else {
        return "Untitled plan".to_owned();
    };
    let mut end = title.len().min(MAX_DOC_TITLE_BYTES);
    while !title.is_char_boundary(end) {
        end -= 1;
    }
    title[..end].trim_end().to_owned()
}

/// The proposals a plan names in a `Proposals: #3, #5` line among its first
/// lines; none without such a line.
pub fn proposals_named(body: &str) -> Result<Vec<u64>, String> {
    let Some(line) = body
        .lines()
        .take(PROPOSALS_WITHIN)
        .find(|line| line.starts_with("Proposals:"))
    else {
        return Ok(Vec::new());
    };
    let malformed = || format!("malformed Proposals line: {line}");
    let list = line["Proposals:".len()..].trim();
    if list.is_empty() {
        return Err(malformed());
    }
    list.split(',')
        .map(|item| {
            let digits = item.trim().strip_prefix('#').ok_or_else(malformed)?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return Err(malformed());
            }
            digits.parse::<u64>().map_err(|_| malformed())
        })
        .collect()
}

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
        doc.body
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
            body: String::new(),
        }
    }

    #[test]
    fn a_plan_is_titled_by_its_first_heading() {
        assert_eq!(
            plan_title("intro\n## Sub\n#  Real title  \n# Second"),
            "Real title"
        );
        assert_eq!(plan_title("no heading\n#nospace"), "Untitled plan");
        assert_eq!(plan_title("# \n"), "Untitled plan");
        // Cut on a character boundary: 'ä' is two bytes.
        let long = format!("# {}", "ä".repeat(150));
        let title = plan_title(&long);
        assert_eq!(title, "ä".repeat(100));
        assert!(title.len() <= MAX_DOC_TITLE_BYTES);
    }

    #[test]
    fn a_plan_names_its_proposals_in_one_line_near_the_top() {
        assert_eq!(
            proposals_named("# T\n\nProposals: #3, #5\n"),
            Ok(vec![3, 5])
        );
        assert_eq!(proposals_named("# T\nProposals:#7\n"), Ok(vec![7]));
        assert_eq!(proposals_named("# T\n\nbody\n"), Ok(Vec::new()));
        // Too far down is body text, not the line.
        let late = format!("# T\n{}Proposals: #1\n", "x\n".repeat(25));
        assert_eq!(proposals_named(&late), Ok(Vec::new()));
        for bad in [
            "Proposals: 3",
            "Proposals:",
            "Proposals: #3,",
            "Proposals: #3 #4",
            "Proposals: #x",
        ] {
            assert_eq!(
                proposals_named(&format!("# T\n{bad}\n")),
                Err(format!("malformed Proposals line: {bad}")),
                "{bad}"
            );
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
