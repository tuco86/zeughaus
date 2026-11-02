//! Tickets as an agent and a script read them. Pure: the clock is an
//! argument.

use zeughaus_link::{DocState, DocView, LinkKind, blocked_by, doc_groups, linked_from};

/// `5m`, `3h`, `2d`: whole minutes below an hour, whole hours below a day,
/// else whole days.
pub fn age(seconds: u64) -> String {
    match seconds {
        0..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// One ticket with its texts; `docs` (the repository's tickets) give the
/// links that point at it:
///
/// ```text
/// #4 ticket review: Store plans in the CI
/// from zeughaus
/// created 3h ago
/// links: depends on #2, related #3; blocks #9
///
/// <body>
///
/// ## Plan (revision 1)
///
/// <plan>
/// ```
pub fn doc_text(doc: &DocView, docs: &[DocView], now: u64) -> String {
    let mut text = format!(
        "#{} {} {}: {}\nfrom {}\ncreated {} ago\n",
        doc.number,
        doc.kind.name(),
        doc.state.name(),
        doc.title,
        doc.source,
        age(now.saturating_sub(doc.created)),
    );
    let outgoing: Vec<String> = doc
        .links
        .iter()
        .map(|link| format!("{} #{}", link.kind.name(), link.number))
        .collect();
    let incoming: Vec<String> = linked_from(doc.number, docs)
        .into_iter()
        .map(|(kind, number)| match kind {
            LinkKind::DependsOn => format!("blocks #{number}"),
            LinkKind::Related => format!("related #{number}"),
        })
        .collect();
    let parts: Vec<String> = [outgoing, incoming]
        .into_iter()
        .filter(|part| !part.is_empty())
        .map(|part| part.join(", "))
        .collect();
    if !parts.is_empty() {
        text.push_str(&format!("links: {}\n", parts.join("; ")));
    }
    text.push('\n');
    let shown = doc.shown_text();
    text.push_str(&shown);
    if !doc.plan_text.is_empty() {
        if !shown.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!(
            "## Plan (revision {})\n\n{}",
            doc.revision, doc.plan_text
        ));
    }
    text
}

/// One line per document, under the heading of its group: awaiting
/// review, the queue in the order a master takes it, in progress,
/// planning, open, then the history. A ticket that waits for its
/// dependencies says for which.
pub fn docs_text(docs: &[DocView], now: u64) -> String {
    let mut text = String::new();
    for (group, members) in doc_groups(docs) {
        text.push_str(&format!("{}:\n", group.title()));
        for doc in members {
            let finished = matches!(
                doc.state,
                DocState::Done | DocState::Dismissed | DocState::Merged
            );
            let blocked = if finished {
                Vec::new()
            } else {
                blocked_by(doc, docs)
            };
            let after = if blocked.is_empty() {
                String::new()
            } else {
                let numbers: Vec<String> = blocked.iter().map(|n| format!("#{n}")).collect();
                format!(" (after {})", numbers.join(", "))
            };
            text.push_str(&format!(
                "  #{} {} {}: {} ({} ago){after}\n",
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
    use zeughaus_link::{DocKind, DocLink};

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
            released: None,
            links: Vec::new(),
            merged: Vec::new(),
            into: None,
            runs_on: Vec::new(),
            placement: Vec::new(),
            body: String::new(),
            plan_text: String::new(),
            revision: 0,
            planner: None,
            feedback: String::new(),
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
    fn a_ticket_reads_as_header_links_proposal_then_plan() {
        let link = |kind, number| DocLink { kind, number };
        let mut ticket = doc(4, DocKind::Ticket, DocState::Review);
        ticket.body = "Please do it.".to_owned();
        ticket.plan_text = "# Doc 4\n\nDo it.\n".to_owned();
        ticket.revision = 2;
        ticket.links = vec![link(LinkKind::DependsOn, 2), link(LinkKind::Related, 3)];
        let mut blocked = doc(9, DocKind::Ticket, DocState::Open);
        blocked.links = vec![link(LinkKind::DependsOn, 4)];
        let docs = [ticket.clone(), blocked];
        assert_eq!(
            doc_text(&ticket, &docs, 1_000 + 3 * 3600),
            "#4 ticket review: Doc 4\nfrom weida\ncreated 3h ago\n\
             links: depends on #2, related #3; blocks #9\n\n\
             Please do it.\n\n## Plan (revision 2)\n\n# Doc 4\n\nDo it.\n"
        );
        let bare = doc(5, DocKind::Ticket, DocState::Open);
        assert_eq!(
            doc_text(&bare, &[], 1_000),
            "#5 ticket open: Doc 5\nfrom weida\ncreated 0m ago\n\n"
        );
    }

    #[test]
    fn the_list_puts_what_waits_for_review_first_and_says_what_blocks() {
        let mut ready = doc(4, DocKind::Ticket, DocState::Ready);
        ready.links = vec![DocLink {
            kind: LinkKind::DependsOn,
            number: 2,
        }];
        let docs = [
            doc(1, DocKind::Ticket, DocState::Done),
            doc(2, DocKind::Ticket, DocState::Open),
            doc(3, DocKind::Ticket, DocState::Review),
            ready,
        ];
        assert_eq!(
            docs_text(&docs, 1_000 + 120),
            "Awaiting review:\n  #3 ticket review: Doc 3 (1m ago)\n\
             Planning:\n  #4 ticket ready: Doc 4 (1m ago) (after #2)\n\
             Open:\n  #2 ticket open: Doc 2 (1m ago)\n\
             History:\n  #1 ticket done: Doc 1 (1m ago)\n"
        );
    }
}
