//! Tickets as an agent and a script read them. Pure: the clock is an
//! argument.

use zeughaus_link::{
    DocState, DocView, LinkKind, TicketComment, TicketSuggestion, doc_groups, linked_from,
};

/// `5m`, `3h`, `2d`: whole minutes below an hour, whole hours below a day,
/// else whole days.
pub fn age(seconds: u64) -> String {
    match seconds {
        0..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// The comments of a ticket as a Markdown section, or nothing without
/// any:
///
/// ```text
/// ## Comments
///
/// - hannes@tuco-pc, 2h ago:
///   <text, each line indented>
/// ```
pub fn comments_text(comments: &[TicketComment], now: u64) -> String {
    if comments.is_empty() {
        return String::new();
    }
    let mut text = "## Comments\n\n".to_owned();
    for comment in comments {
        text.push_str(&format!(
            "- {}, {} ago:\n",
            comment.author,
            age(now.saturating_sub(comment.at))
        ));
        for line in comment.text.lines() {
            if line.is_empty() {
                text.push('\n');
            } else {
                text.push_str(&format!("  {line}\n"));
            }
        }
    }
    text
}

/// `<headline> (<model>, <age> ago): <reason>`, and for a rewrite the new
/// text after a blank line.
pub fn suggestion_text(suggestion: &TicketSuggestion, now: u64) -> String {
    let mut text = format!(
        "{} ({}, {} ago): {}\n",
        suggestion.headline(),
        suggestion.model,
        age(now.saturating_sub(suggestion.created)),
        suggestion.reason
    );
    if !suggestion.text.is_empty() {
        text.push('\n');
        text.push_str(&suggestion.text);
    }
    text
}

/// One ticket with its texts; `docs` (the repository's tickets) give the
/// links that point at it:
///
/// ```text
/// #4 ticket review: Store plans in the CI
/// from zeughaus
/// created 3h ago
/// links: depends on #2, related #weida-3; blocks #9
/// filed: #weida-7 (Keep the corpus)
/// suggestion: dismiss it (claude-haiku-5-5, 2m ago): <reason>
///
/// <body>
///
/// ## Comments
///
/// - hannes@tuco-pc, 1h ago:
///   <comment>
///
/// ## Plan (revision 1)
///
/// <plan>
///
/// ## Report (2h ago)
///
/// <report>
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
        .map(|link| format!("{} {}", link.kind.name(), link.reference(&doc.repo)))
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
    if !doc.filed.is_empty() {
        let filed: Vec<String> = doc
            .filed
            .iter()
            .map(|ticket| {
                let reference = if ticket.repo == doc.repo {
                    format!("#{}", ticket.number)
                } else {
                    format!("#{}-{}", ticket.repo, ticket.number)
                };
                format!("{reference} ({})", ticket.title)
            })
            .collect();
        text.push_str(&format!("filed: {}\n", filed.join(", ")));
    }
    if let Some(suggestion) = &doc.suggestion {
        let line = suggestion_text(suggestion, now);
        text.push_str(&format!(
            "suggestion: {}\n",
            line.lines().next().unwrap_or_default()
        ));
    }
    text.push('\n');
    let mut parts = vec![doc.shown_text().trim_end().to_owned()];
    parts.push(comments_text(&doc.comments, now).trim_end().to_owned());
    if !doc.plan_text.is_empty() {
        parts.push(format!(
            "## Plan (revision {})\n\n{}",
            doc.revision,
            doc.plan_text.trim_end()
        ));
    }
    if let Some(report) = &doc.report {
        let from = if report.from_transcript {
            "from the master's transcript, "
        } else {
            ""
        };
        parts.push(format!(
            "## Report ({from}{} ago)\n\n{}",
            age(now.saturating_sub(report.written)),
            report.text.trim_end()
        ));
    }
    parts.retain(|part| !part.is_empty());
    if !parts.is_empty() {
        text.push_str(&parts.join("\n\n"));
        text.push('\n');
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
            let after = if finished || doc.waits_for.is_empty() {
                String::new()
            } else {
                let refs: Vec<String> = doc
                    .waits_for
                    .iter()
                    .map(|link| link.reference(&doc.repo))
                    .collect();
                format!(" (after {})", refs.join(", "))
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
    use zeughaus_link::{DocKind, DocLink, FiledTicket, TicketReport};

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
            auto_approve: false,
            feedback: String::new(),
            onboard: None,
            note: String::new(),
            waits_for: Vec::new(),
            filed: Vec::new(),
            comments: Vec::new(),
            suggestion: None,
            report: None,
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
        let link = |kind, number, repo: Option<&str>| DocLink {
            kind,
            number,
            repo: repo.map(str::to_owned),
        };
        let mut ticket = doc(4, DocKind::Ticket, DocState::Review);
        ticket.body = "# Doc 4\n\nPlease do it.".to_owned();
        ticket.plan_text = "# Doc 4\n\nDo it.\n".to_owned();
        ticket.revision = 2;
        ticket.links = vec![
            link(LinkKind::DependsOn, 2, None),
            link(LinkKind::Related, 3, Some("weida")),
        ];
        ticket.filed = vec![FiledTicket {
            repo: "weida".to_owned(),
            number: 7,
            title: "Keep the corpus".to_owned(),
        }];
        let mut blocked = doc(9, DocKind::Ticket, DocState::Open);
        blocked.links = vec![link(LinkKind::DependsOn, 4, None)];
        let docs = [ticket.clone(), blocked];
        assert_eq!(
            doc_text(&ticket, &docs, 1_000 + 3 * 3600),
            "#4 ticket review: Doc 4\nfrom weida\ncreated 3h ago\n\
             links: depends on #2, related #weida-3; blocks #9\n\
             filed: #weida-7 (Keep the corpus)\n\n\
             Please do it.\n\n## Plan (revision 2)\n\n# Doc 4\n\nDo it.\n"
        );
        let bare = doc(5, DocKind::Ticket, DocState::Open);
        assert_eq!(
            doc_text(&bare, &[], 1_000),
            "#5 ticket open: Doc 5\nfrom weida\ncreated 0m ago\n\n"
        );
    }

    #[test]
    fn a_done_ticket_ends_with_its_report() {
        let mut ticket = doc(6, DocKind::Ticket, DocState::Done);
        ticket.plan_text = "# Doc 6\n\nDo it.\n".to_owned();
        ticket.revision = 1;
        ticket.report = Some(TicketReport {
            text: "Did it.".into(),
            written: 1_000,
            from_transcript: false,
        });
        assert_eq!(
            doc_text(&ticket, &[], 1_000 + 7200),
            "#6 ticket done: Doc 6\nfrom weida\ncreated 2h ago\n\n\
             ## Plan (revision 1)\n\n# Doc 6\n\nDo it.\n\n## Report (2h ago)\n\nDid it.\n"
        );
        if let Some(report) = &mut ticket.report {
            report.from_transcript = true;
        }
        assert!(
            doc_text(&ticket, &[], 1_000 + 7200)
                .ends_with("## Report (from the master's transcript, 2h ago)\n\nDid it.\n")
        );
    }

    #[test]
    fn the_list_puts_what_waits_for_review_first_and_says_what_blocks() {
        let mut ready = doc(4, DocKind::Ticket, DocState::Ready);
        ready.waits_for = vec![
            DocLink {
                kind: LinkKind::DependsOn,
                number: 2,
                repo: None,
            },
            DocLink {
                kind: LinkKind::DependsOn,
                number: 5,
                repo: Some("weida".to_owned()),
            },
        ];
        let docs = [
            doc(1, DocKind::Ticket, DocState::Done),
            doc(2, DocKind::Ticket, DocState::Open),
            doc(3, DocKind::Ticket, DocState::Review),
            ready,
        ];
        assert_eq!(
            docs_text(&docs, 1_000 + 120),
            "Awaiting review:\n  #3 ticket review: Doc 3 (1m ago)\n\
             Planning:\n  #4 ticket ready: Doc 4 (1m ago) (after #2, #weida-5)\n\
             Open:\n  #2 ticket open: Doc 2 (1m ago)\n\
             History:\n  #1 ticket done: Doc 1 (1m ago)\n"
        );
    }
}
