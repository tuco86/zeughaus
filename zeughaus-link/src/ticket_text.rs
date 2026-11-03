//! The Markdown form of a ticket, and the sections of a plan that become
//! tickets of their own.
//!
//! A ticket is one text: its first non-empty line `# <title>`, then among
//! the lines under it `Depends on:` (alias `Blocked by:`) and `Related:`
//! lines of references, `#12` for a ticket of the same repository and
//! `#weida-12` for one of another. A plan carries the work of another
//! repository, or a later ticket of its own, in a section
//! `## <repo> - <title> [#label]`; the CI files each as a ticket of that
//! repository when the plan is approved, and its link lines may also name
//! `#self` (the plan's ticket) and the label of another section. Everything
//! here is pure.

use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::ci::{DocLink, HEADER_LINES, LinkKind, MAX_DOC_TITLE_BYTES};

/// One reference as written after its `#`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketRef {
    /// `#12`: a ticket of the same repository.
    Same(u64),
    /// `#weida-12`: a ticket of another repository.
    Other { repo: String, number: u64 },
    /// `#self`: the plan's own ticket; in a plan's section only.
    SelfRef,
    /// `#fuzz`: a labelled section of the same plan; in a plan's section
    /// only.
    Label(String),
}

/// What a ticket's text says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketText {
    pub title: String,
    pub links: Vec<DocLink>,
}

/// A `## <repo> - <title> [#label]` heading whose `<repo>` is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalHeading {
    pub repo: String,
    /// Without the label.
    pub title: String,
    pub label: Option<String>,
}

/// One section of a plan for a ticket of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSection {
    pub repo: String,
    pub title: String,
    pub label: Option<String>,
    pub depends: Vec<TicketRef>,
    pub related: Vec<TicketRef>,
    /// The section's lines without its heading and link lines, trimmed.
    pub text: String,
}

/// A plan's section as it was filed: the ticket it became.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiledTicket {
    pub repo: String,
    pub number: u64,
    /// The section's title, which is also the ticket's.
    pub title: String,
}

/// `[a-z0-9][a-z0-9-]*`, the names of `ci.toml`'s repositories.
pub(crate) fn repo_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `[a-z][a-z0-9_]*`, a section's label (`self` included; the callers
/// refuse it).
fn label_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Digits only, at least 1.
fn ticket_number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse::<u64>().ok().filter(|&n| n >= 1)
}

/// The reference `token` (its `#` included) stands for, or `None` when it
/// is none.
pub fn parse_ref(token: &str) -> Option<TicketRef> {
    let name = token.strip_prefix('#')?;
    if let Some(number) = ticket_number(name) {
        return Some(TicketRef::Same(number));
    }
    if name == "self" {
        return Some(TicketRef::SelfRef);
    }
    if let Some((repo, number)) = name.rsplit_once('-')
        && repo_name(repo)
        && let Some(number) = ticket_number(number)
    {
        return Some(TicketRef::Other {
            repo: repo.to_owned(),
            number,
        });
    }
    label_name(name).then(|| TicketRef::Label(name.to_owned()))
}

/// The lines of `text` with their byte ranges (line break included), each
/// without its line break.
fn lines(text: &str) -> impl Iterator<Item = (Range<usize>, &str)> {
    let mut at = 0;
    text.split_inclusive('\n').map(move |raw| {
        let start = at;
        at += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        (start..at, line.strip_suffix('\r').unwrap_or(line))
    })
}

/// A line that opens or closes fenced code.
fn is_fence(line: &str) -> bool {
    line.starts_with("```") || line.starts_with("~~~")
}

/// A link line's key as written, the kind it stands for, and its list.
fn link_line(line: &str) -> Option<(&'static str, LinkKind, &str)> {
    [
        ("Depends on", LinkKind::DependsOn),
        ("Blocked by", LinkKind::DependsOn),
        ("Related", LinkKind::Related),
    ]
    .into_iter()
    .find_map(|(key, kind)| {
        let list = line.strip_prefix(key)?.strip_prefix(':')?;
        Some((key, kind, list))
    })
}

/// The references of a block's link lines by kind, and the indices of
/// those lines.
type HeaderLinks = (Vec<(LinkKind, TicketRef)>, Vec<usize>);

/// The references of the link lines among the first [`HEADER_LINES`] of
/// `lines` (those under a title or heading), outside fenced code.
fn header_links(lines: &[&str]) -> Result<HeaderLinks, String> {
    let mut refs = Vec::new();
    let mut at = Vec::new();
    let mut fenced = false;
    for (index, &line) in lines.iter().take(HEADER_LINES).enumerate() {
        if is_fence(line) {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let Some((key, kind, list)) = link_line(line) else {
            continue;
        };
        let malformed = || format!("malformed {key} line: {line}");
        let list = list.trim();
        if list.is_empty() {
            return Err(malformed());
        }
        for item in list.split(',') {
            let item = item.trim();
            if item.contains(char::is_whitespace) {
                return Err(malformed());
            }
            refs.push((kind, parse_ref(item).ok_or_else(malformed)?));
        }
        at.push(index);
    }
    Ok((refs, at))
}

/// The title and links a ticket's text gives, or why it is refused.
pub fn ticket_text(body: &str) -> Result<TicketText, String> {
    let all: Vec<&str> = lines(body).map(|(_, line)| line).collect();
    let missing = || "a ticket's text starts with its \"# <title>\" line".to_owned();
    let first = all
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or_else(missing)?;
    let title = all[first].strip_prefix("# ").ok_or_else(missing)?.trim();
    if title.is_empty() || title.len() > MAX_DOC_TITLE_BYTES {
        return Err(format!(
            "a title is one line of 1 to {MAX_DOC_TITLE_BYTES} bytes"
        ));
    }
    let (refs, _) = header_links(&all[first + 1..])?;
    let links = refs
        .into_iter()
        .map(|(kind, link)| match link {
            TicketRef::Same(number) => Ok(DocLink {
                kind,
                number,
                repo: None,
            }),
            TicketRef::Other { repo, number } => Ok(DocLink {
                kind,
                number,
                repo: Some(repo),
            }),
            TicketRef::SelfRef => Err("#self works only in a plan's section".to_owned()),
            TicketRef::Label(label) => Err(format!("#{label} works only in a plan's section")),
        })
        .collect::<Result<_, _>>()?;
    Ok(TicketText {
        title: title.to_owned(),
        links,
    })
}

/// `body` after its first non-empty line when that line is its `# ` title,
/// without the line breaks that follow; otherwise `body` as it is.
pub fn text_without_title(body: &str) -> &str {
    let Some((range, line)) = lines(body).find(|(_, line)| !line.trim().is_empty()) else {
        return body;
    };
    if line.starts_with("# ") {
        body[range.end..].trim_start_matches(['\n', '\r'])
    } else {
        body
    }
}

/// The section heading `heading` (the text after `## `) stands for: a
/// repository `is_repo` knows, ` - `, a title, optionally ending in a label
/// `#<label>`. `None` for an ordinary heading.
pub fn proposal_heading(heading: &str, is_repo: impl Fn(&str) -> bool) -> Option<ProposalHeading> {
    let (repo, title) = heading.split_once(" - ")?;
    let repo = repo.trim();
    if !is_repo(repo) {
        return None;
    }
    let mut title = title.trim();
    let mut label = None;
    if let Some(last) = title.split_whitespace().last()
        && let Some(name) = last.strip_prefix('#')
        && label_name(name)
    {
        label = Some(name.to_owned());
        title = title[..title.len() - last.len()].trim_end();
    }
    Some(ProposalHeading {
        repo: repo.to_owned(),
        title: title.to_owned(),
        label,
    })
}

/// Every section of `plan` for a ticket of its own, its heading line
/// included: from its heading to the next `# ` or `## ` line outside
/// fenced code, or to the end.
fn section_spans(
    plan: &str,
    is_repo: &dyn Fn(&str) -> bool,
) -> Vec<(Range<usize>, ProposalHeading)> {
    let mut spans = Vec::new();
    let mut open: Option<(usize, ProposalHeading)> = None;
    let mut fenced = false;
    for (range, line) in lines(plan) {
        if is_fence(line) {
            fenced = !fenced;
            continue;
        }
        if fenced || !(line.starts_with("# ") || line.starts_with("## ")) {
            continue;
        }
        if let Some((start, heading)) = open.take() {
            spans.push((start..range.start, heading));
        }
        if let Some(heading) = line
            .strip_prefix("## ")
            .and_then(|heading| proposal_heading(heading, is_repo))
        {
            open = Some((range.start, heading));
        }
    }
    if let Some((start, heading)) = open {
        spans.push((start..plan.len(), heading));
    }
    spans
}

/// The labelled sections each section names, by index, in link order.
fn named_sections(sections: &[PlanSection]) -> Vec<Vec<usize>> {
    let index: BTreeMap<&str, usize> = sections
        .iter()
        .enumerate()
        .filter_map(|(at, section)| Some((section.label.as_deref()?, at)))
        .collect();
    sections
        .iter()
        .map(|section| {
            section
                .depends
                .iter()
                .chain(&section.related)
                .filter_map(|link| match link {
                    TicketRef::Label(label) => index.get(label.as_str()).copied(),
                    _ => None,
                })
                .collect()
        })
        .collect()
}

/// A cycle through the sections' labels, as the indices on it.
fn label_cycle(names: &[Vec<usize>]) -> Option<Vec<usize>> {
    fn visit(
        at: usize,
        names: &[Vec<usize>],
        state: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<usize>> {
        state[at] = 1;
        stack.push(at);
        for &next in &names[at] {
            match state[next] {
                0 => {
                    if let Some(cycle) = visit(next, names, state, stack) {
                        return Some(cycle);
                    }
                }
                1 => {
                    let from = stack.iter().position(|&s| s == next).unwrap_or(0);
                    return Some(stack[from..].to_vec());
                }
                _ => {}
            }
        }
        stack.pop();
        state[at] = 2;
        None
    }
    let mut state = vec![0u8; names.len()];
    let mut stack = Vec::new();
    (0..names.len()).find_map(|start| {
        if state[start] != 0 {
            return None;
        }
        visit(start, names, &mut state, &mut stack)
    })
}

/// The sections of `plan` for tickets of their own, whose repositories
/// `is_repo` knows, in plan order, or why the plan is refused: a title out
/// of bounds, the label `self`, a label used twice, a label no section
/// has, a section naming itself, or sections waiting for each other.
pub fn plan_sections(
    plan: &str,
    is_repo: impl Fn(&str) -> bool,
) -> Result<Vec<PlanSection>, String> {
    let mut sections = Vec::new();
    for (range, heading) in section_spans(plan, &is_repo) {
        let all: Vec<&str> = lines(&plan[range]).map(|(_, line)| line).collect();
        let heading_line = all[0];
        if heading.title.is_empty() || heading.title.len() > MAX_DOC_TITLE_BYTES {
            return Err(format!(
                "a section title is 1 to {MAX_DOC_TITLE_BYTES} bytes: {heading_line}"
            ));
        }
        if heading.label.as_deref() == Some("self") {
            return Err(format!("\"self\" is no label: {heading_line}"));
        }
        let (refs, at) = header_links(&all[1..])?;
        let mut depends = Vec::new();
        let mut related = Vec::new();
        for (kind, link) in refs {
            match kind {
                LinkKind::DependsOn => depends.push(link),
                LinkKind::Related => related.push(link),
            }
        }
        let text: Vec<&str> = all[1..]
            .iter()
            .enumerate()
            .filter(|(index, _)| !at.contains(index))
            .map(|(_, &line)| line)
            .collect();
        sections.push(PlanSection {
            repo: heading.repo,
            title: heading.title,
            label: heading.label,
            depends,
            related,
            text: text.join("\n").trim().to_owned(),
        });
    }
    let mut labels: Vec<&str> = Vec::new();
    for label in sections.iter().filter_map(|s| s.label.as_deref()) {
        if labels.contains(&label) {
            return Err(format!("label #{label} names two sections"));
        }
        labels.push(label);
    }
    for section in &sections {
        for link in section.depends.iter().chain(&section.related) {
            if let TicketRef::Label(label) = link
                && !labels.contains(&label.as_str())
            {
                return Err(format!("#{label} names no section of this plan"));
            }
        }
    }
    for section in &sections {
        let Some(own) = &section.label else {
            continue;
        };
        if section
            .depends
            .iter()
            .chain(&section.related)
            .any(|link| matches!(link, TicketRef::Label(label) if label == own))
        {
            return Err(format!("section \"{}\" depends on itself", section.title));
        }
    }
    // A related section is filed before the one naming it too, so a cycle
    // of any links could not be filed.
    if let Some(mut cycle) = label_cycle(&named_sections(&sections)) {
        cycle.sort_unstable();
        let label = |at: usize| sections[at].label.clone().unwrap_or_default();
        return Err(format!(
            "sections #{} and #{} wait for each other",
            label(cycle[0]),
            label(cycle[1])
        ));
    }
    Ok(sections)
}

/// The order sections are filed in: plan order, except that a section
/// comes after every labelled section it names.
pub fn filing_order(sections: &[PlanSection]) -> Vec<usize> {
    let names = named_sections(sections);
    let mut placed = vec![false; sections.len()];
    let mut order = Vec::with_capacity(sections.len());
    while order.len() < sections.len() {
        let ready = |at: usize| !placed[at] && names[at].iter().all(|&n| placed[n] || n == at);
        // A cycle was refused when the plan was written; should one be
        // there anyway, the rest follow in plan order.
        let Some(next) = (0..sections.len())
            .find(|&at| ready(at))
            .or_else(|| (0..sections.len()).find(|&at| !placed[at]))
        else {
            break;
        };
        placed[next] = true;
        order.push(next);
    }
    order
}

/// A ticket's text: its title line, its link lines, a blank line and
/// `rest`, trimmed, ending in one line break. `links` are in the ticket's
/// own terms: `repo: None` is its own repository. A kind without links
/// gets no line.
pub fn compose_text(title: &str, links: &[DocLink], rest: &str) -> String {
    let mut text = format!("# {}\n", title.trim());
    for (key, kind) in [
        ("Depends on", LinkKind::DependsOn),
        ("Related", LinkKind::Related),
    ] {
        let refs: Vec<String> = links
            .iter()
            .filter(|link| link.kind == kind)
            .map(|link| link.reference(""))
            .collect();
        if !refs.is_empty() {
            text.push_str(&format!("{key}: {}\n", refs.join(", ")));
        }
    }
    let rest = rest.trim();
    if !rest.is_empty() {
        text.push('\n');
        text.push_str(rest);
        text.push('\n');
    }
    text
}

/// The text `section` of ticket `plan_number` of `plan_repo` is filed as
/// in its repository: its references resolved (`#self` to the plan's
/// ticket, a label through `labels`, the sections filed so far) and
/// written relative to that repository, plus a `Related` link to the plan's
/// ticket unless it is linked already. Without `links_allowed` it gets no
/// link lines at all.
pub fn section_text(
    section: &PlanSection,
    plan_repo: &str,
    plan_number: u64,
    labels: &BTreeMap<String, (String, u64)>,
    links_allowed: bool,
) -> Result<String, String> {
    let mut links: Vec<DocLink> = Vec::new();
    if links_allowed {
        let refs = section
            .depends
            .iter()
            .map(|link| (LinkKind::DependsOn, link))
            .chain(section.related.iter().map(|link| (LinkKind::Related, link)));
        for (kind, link) in refs {
            let (repo, number) = match link {
                TicketRef::Same(number) => (plan_repo.to_owned(), *number),
                TicketRef::Other { repo, number } => (repo.clone(), *number),
                TicketRef::SelfRef => (plan_repo.to_owned(), plan_number),
                TicketRef::Label(label) => labels
                    .get(label)
                    .cloned()
                    .ok_or_else(|| format!("#{label} is not filed yet"))?,
            };
            let link = DocLink {
                kind,
                number,
                repo: (repo != section.repo).then_some(repo),
            };
            if !links.contains(&link) {
                links.push(link);
            }
        }
        let plan_ticket = DocLink {
            kind: LinkKind::Related,
            number: plan_number,
            repo: (plan_repo != section.repo).then(|| plan_repo.to_owned()),
        };
        if !links
            .iter()
            .any(|link| link.number == plan_ticket.number && link.repo == plan_ticket.repo)
        {
            links.push(plan_ticket);
        }
    }
    Ok(compose_text(&section.title, &links, &section.text))
}

/// `plan` without the sections that were filed as tickets of their own,
/// everything else byte for byte.
pub fn without_sections(plan: &str, filed: &[FiledTicket]) -> String {
    let is_repo = |name: &str| filed.iter().any(|ticket| ticket.repo == name);
    let mut kept = String::with_capacity(plan.len());
    let mut at = 0;
    for (range, heading) in section_spans(plan, &is_repo) {
        if filed
            .iter()
            .any(|ticket| ticket.repo == heading.repo && ticket.title == heading.title)
        {
            kept.push_str(&plan[at..range.start]);
            at = range.end;
        }
    }
    kept.push_str(&plan[at..]);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(kind: LinkKind, number: u64, repo: Option<&str>) -> DocLink {
        DocLink {
            kind,
            number,
            repo: repo.map(str::to_owned),
        }
    }

    fn known(name: &str) -> bool {
        matches!(name, "weida" | "zeughaus" | "demo")
    }

    #[test]
    fn every_reference_form_parses_and_malformed_ones_do_not() {
        assert_eq!(parse_ref("#12"), Some(TicketRef::Same(12)));
        assert_eq!(
            parse_ref("#weida-12"),
            Some(TicketRef::Other {
                repo: "weida".to_owned(),
                number: 12
            })
        );
        // Split at the last dash: a repository name may contain dashes.
        assert_eq!(
            parse_ref("#scan-rs-3"),
            Some(TicketRef::Other {
                repo: "scan-rs".to_owned(),
                number: 3
            })
        );
        assert_eq!(parse_ref("#self"), Some(TicketRef::SelfRef));
        assert_eq!(
            parse_ref("#fuzz_2"),
            Some(TicketRef::Label("fuzz_2".to_owned()))
        );
        for bad in [
            "12", "#", "#0", "#weida-0", "#weida-", "#Weida-1", "#-1", "#2x", "#fuzz-x", "#a b",
            "#_x",
        ] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_ticket_text_gives_its_title_and_links() {
        let text = ticket_text(
            "\n# Keep the corpus \nDepends on: #3, #weida-12\nBlocked by: #4\nRelated: #1\n\nBody\n",
        )
        .unwrap();
        assert_eq!(text.title, "Keep the corpus");
        assert_eq!(
            text.links,
            vec![
                link(LinkKind::DependsOn, 3, None),
                link(LinkKind::DependsOn, 12, Some("weida")),
                link(LinkKind::DependsOn, 4, None),
                link(LinkKind::Related, 1, None),
            ]
        );
    }

    #[test]
    fn a_ticket_text_without_its_title_or_with_a_long_one_is_refused() {
        let missing = Err("a ticket's text starts with its \"# <title>\" line".to_owned());
        assert_eq!(ticket_text(""), missing);
        assert_eq!(ticket_text("Body\n# Title\n"), missing);
        assert_eq!(ticket_text("## Title\n"), missing);
        let long = format!("# {}\n", "x".repeat(MAX_DOC_TITLE_BYTES + 1));
        assert_eq!(
            ticket_text(&long),
            Err(format!(
                "a title is one line of 1 to {MAX_DOC_TITLE_BYTES} bytes"
            ))
        );
        let longest = format!("# {}\n", "x".repeat(MAX_DOC_TITLE_BYTES));
        assert!(ticket_text(&longest).is_ok());
    }

    #[test]
    fn malformed_link_lines_and_plan_references_are_refused() {
        assert_eq!(
            ticket_text("# T\nDepends on:\n"),
            Err("malformed Depends on line: Depends on:".to_owned())
        );
        assert_eq!(
            ticket_text("# T\nBlocked by: #1 #2\n"),
            Err("malformed Blocked by line: Blocked by: #1 #2".to_owned())
        );
        assert_eq!(
            ticket_text("# T\nRelated: #1, x\n"),
            Err("malformed Related line: Related: #1, x".to_owned())
        );
        assert_eq!(
            ticket_text("# T\nDepends on: #self\n"),
            Err("#self works only in a plan's section".to_owned())
        );
        assert_eq!(
            ticket_text("# T\nRelated: #fuzz\n"),
            Err("#fuzz works only in a plan's section".to_owned())
        );
    }

    #[test]
    fn link_lines_count_under_the_title_and_outside_fenced_code_only() {
        let fenced = "# T\n```\nDepends on: #2\n```\n~~~\nRelated: nonsense\n~~~\n";
        assert!(ticket_text(fenced).unwrap().links.is_empty());
        let mut far = String::from("# T\n");
        far.push_str(&"line\n".repeat(HEADER_LINES));
        far.push_str("Depends on: #2\n");
        assert!(ticket_text(&far).unwrap().links.is_empty());
    }

    #[test]
    fn the_text_without_its_title_is_what_follows_the_title_line() {
        assert_eq!(text_without_title("\n# T\n\nBody\n"), "Body\n");
        assert_eq!(text_without_title("Body\n"), "Body\n");
        assert_eq!(text_without_title("# T"), "");
    }

    const PLAN: &str = "# Job state: data a CI job keeps from one pipeline to the next\n\
        \n\
        ## Context\n\
        \n\
        Why.\n\
        \n\
        ## weida - Keep the fuzz corpus across nightlies in a CI job state store #fuzz\n\
        Depends on: #self\n\
        \n\
        fuzz.sh restores each target's corpus from the store ...\n\
        \n\
        ## demo - Follow up\n\
        Depends on: #fuzz\n\
        Related: #weida-3\n\
        \n\
        More.\n\
        \n\
        ## Verification\n\
        \n\
        ```\n\
        ## weida - Not a section\n\
        ```\n";

    #[test]
    fn a_plan_has_sections_for_known_repositories_only() {
        let sections = plan_sections(PLAN, known).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(
            sections[0],
            PlanSection {
                repo: "weida".to_owned(),
                title: "Keep the fuzz corpus across nightlies in a CI job state store".to_owned(),
                label: Some("fuzz".to_owned()),
                depends: vec![TicketRef::SelfRef],
                related: Vec::new(),
                text: "fuzz.sh restores each target's corpus from the store ...".to_owned(),
            }
        );
        assert_eq!(sections[1].title, "Follow up");
        assert_eq!(sections[1].label, None);
        assert_eq!(
            sections[1].depends,
            vec![TicketRef::Label("fuzz".to_owned())]
        );
        assert_eq!(sections[1].text, "More.");
        // Without weida known, its section is an ordinary one.
        let only_demo = plan_sections(PLAN, |name| name == "demo");
        assert_eq!(
            only_demo,
            Err("#fuzz names no section of this plan".to_owned())
        );
    }

    #[test]
    fn a_plan_with_unresolvable_sections_is_refused() {
        let refused = |plan: &str| plan_sections(plan, known).unwrap_err();
        assert_eq!(
            refused("## weida - #fuzz\n"),
            "a section title is 1 to 200 bytes: ## weida - #fuzz"
        );
        assert_eq!(
            refused("## weida - A #self\n"),
            "\"self\" is no label: ## weida - A #self"
        );
        assert_eq!(
            refused("## weida - A #x\n## demo - B #x\n"),
            "label #x names two sections"
        );
        assert_eq!(
            refused("## weida - A #x\nDepends on: #y\n"),
            "#y names no section of this plan"
        );
        assert_eq!(
            refused("## weida - A #x\nDepends on: #x\n"),
            "section \"A\" depends on itself"
        );
        assert_eq!(
            refused(
                "## weida - A #a\n## demo - B #b\nDepends on: #c\n## demo - C #c\nBlocked by: #b\n"
            ),
            "sections #b and #c wait for each other"
        );
    }

    #[test]
    fn a_section_is_filed_after_the_sections_it_names() {
        let plan = "## demo - A\nDepends on: #b\n## weida - B #b\nRelated: #c\n## demo - C #c\n";
        let sections = plan_sections(plan, known).unwrap();
        assert_eq!(filing_order(&sections), vec![2, 1, 0]);
        let sections = plan_sections(PLAN, known).unwrap();
        assert_eq!(filing_order(&sections), vec![0, 1]);
    }

    #[test]
    fn a_composed_text_reads_back_as_the_same_title_and_links() {
        let links = vec![
            link(LinkKind::DependsOn, 22, Some("zeughaus")),
            link(LinkKind::DependsOn, 4, None),
            link(LinkKind::Related, 1, None),
        ];
        let text = compose_text(" Title ", &links, "\n\nBody\n\n");
        assert_eq!(
            text,
            "# Title\nDepends on: #zeughaus-22, #4\nRelated: #1\n\nBody\n"
        );
        let read = ticket_text(&text).unwrap();
        assert_eq!((read.title.as_str(), read.links), ("Title", links));
        assert_eq!(compose_text("T", &[], ""), "# T\n");
    }

    #[test]
    fn a_section_is_filed_with_its_references_resolved_for_its_repository() {
        let sections = plan_sections(PLAN, known).unwrap();
        let mut labels = BTreeMap::new();
        assert_eq!(
            section_text(&sections[0], "zeughaus", 22, &labels, true).unwrap(),
            "# Keep the fuzz corpus across nightlies in a CI job state store\n\
             Depends on: #zeughaus-22\n\
             \n\
             fuzz.sh restores each target's corpus from the store ...\n"
        );
        assert_eq!(
            section_text(&sections[1], "zeughaus", 22, &labels, true),
            Err("#fuzz is not filed yet".to_owned())
        );
        labels.insert("fuzz".to_owned(), ("weida".to_owned(), 7));
        assert_eq!(
            section_text(&sections[1], "zeughaus", 22, &labels, true).unwrap(),
            "# Follow up\nDepends on: #weida-7\nRelated: #weida-3, #zeughaus-22\n\nMore.\n"
        );
        assert_eq!(
            section_text(&sections[1], "zeughaus", 22, &labels, false).unwrap(),
            "# Follow up\n\nMore.\n"
        );
        // A later ticket of the plan's own repository.
        let own = plan_sections("## zeughaus - Later\nDepends on: #self, #3\n", known).unwrap();
        assert_eq!(
            section_text(&own[0], "zeughaus", 22, &labels, true).unwrap(),
            "# Later\nDepends on: #22, #3\n"
        );
    }

    #[test]
    fn a_plan_without_its_filed_sections_keeps_everything_else() {
        let filed = [FiledTicket {
            repo: "weida".to_owned(),
            number: 7,
            title: "Keep the fuzz corpus across nightlies in a CI job state store".to_owned(),
        }];
        let kept = without_sections(PLAN, &filed);
        assert!(!kept.contains("fuzz.sh"), "{kept}");
        assert!(kept.starts_with("# Job state: data"));
        assert!(kept.contains("## Context\n\nWhy.\n\n## demo - Follow up\n"));
        assert!(kept.ends_with("## weida - Not a section\n```\n"));
        assert_eq!(without_sections(PLAN, &[]), PLAN);
    }
}
