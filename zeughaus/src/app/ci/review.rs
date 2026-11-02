//! The review of a ticket's plan in its popout: the plan split at its
//! headings into sections, each deleted with what is under it or annotated,
//! an overall note, and the text a Refine sends the plan session.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use iced::widget::text_editor;

/// One part of a plan: a heading and the text up to the next heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Section {
    /// 1 to 6 for `#` to `######`; 0 for the text before the first heading.
    pub(super) level: u8,
    pub(super) title: String,
    /// The titles of the enclosing headings and its own, joined by ` > `.
    pub(super) path: String,
    /// Its bytes in the plan: from its heading to the next heading of any
    /// level.
    pub(super) range: Range<usize>,
    /// Where what belongs to it ends: the next heading of the same or a
    /// higher level, or the plan's end.
    pub(super) subtree_end: usize,
}

/// The level and title of an ATX heading line (`## Title`), if it is one.
fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim_end();
    Some((hashes as u8, title))
}

/// `plan` split at its headings outside fenced code. Text before the first
/// heading is a `Preamble` section of level 0, left out when it is blank.
pub(super) fn sections(plan: &str) -> Vec<Section> {
    let mut starts: Vec<(usize, u8, String)> = Vec::new();
    let mut fenced = false;
    let mut at = 0;
    for line in plan.split_inclusive('\n') {
        let bare = line.trim_end_matches(['\n', '\r']);
        if bare.starts_with("```") || bare.starts_with("~~~") {
            fenced = !fenced;
        } else if !fenced && let Some((level, title)) = heading(bare) {
            starts.push((at, level, title.to_owned()));
        }
        at += line.len();
    }
    let mut sections = Vec::with_capacity(starts.len() + 1);
    let first = starts.first().map_or(plan.len(), |(start, _, _)| *start);
    if !plan[..first].trim().is_empty() {
        sections.push(Section {
            level: 0,
            title: "Preamble".to_owned(),
            path: "Preamble".to_owned(),
            range: 0..first,
            subtree_end: first,
        });
    }
    // The titles of the headings above, by level. The plan's title, its
    // first `# ` heading, encloses everything and names nothing: a path
    // leaves it out.
    let title_at = starts.iter().position(|(_, level, _)| *level == 1);
    let mut above: Vec<(u8, usize, String)> = Vec::new();
    for (i, (start, level, title)) in starts.iter().enumerate() {
        let end = starts.get(i + 1).map_or(plan.len(), |(next, _, _)| *next);
        let subtree_end = starts[i + 1..]
            .iter()
            .find(|(_, other, _)| other <= level)
            .map_or(plan.len(), |(next, _, _)| *next);
        above.retain(|(other, _, _)| other < level);
        above.push((*level, i, title.clone()));
        let named: Vec<&str> = above
            .iter()
            .filter(|(_, at, _)| Some(*at) != title_at || *at == i)
            .map(|(_, _, title)| title.as_str())
            .collect();
        let path = named.join(" > ");
        sections.push(Section {
            level: *level,
            title: title.clone(),
            path,
            range: *start..end,
            subtree_end,
        });
    }
    sections
}

/// Whether section `at` lies inside the subtree of section `of` (and is not
/// `of` itself).
pub(super) fn inside(sections: &[Section], at: usize, of: usize) -> bool {
    let (Some(section), Some(parent)) = (sections.get(at), sections.get(of)) else {
        return false;
    };
    at != of
        && parent.level > 0
        && section.range.start > parent.range.start
        && section.range.start < parent.subtree_end
}

/// Whether section `at` is deleted, itself or with a section it lies in.
pub(super) fn deleted_at(sections: &[Section], deleted: &BTreeSet<usize>, at: usize) -> bool {
    deleted
        .iter()
        .any(|&gone| gone == at || inside(sections, at, gone))
}

/// The review a Refine sends: the overall note, the sections to delete
/// (one inside a deleted section is not listed again) and the notes on the
/// sections that stay; `""` when there is nothing.
pub(super) fn refine_text(
    sections: &[Section],
    deleted: &BTreeSet<usize>,
    notes: &BTreeMap<usize, String>,
    overall: &str,
    revision: u32,
) -> String {
    let mut parts = Vec::new();
    let overall = overall.trim();
    if !overall.is_empty() {
        parts.push(format!("Overall:\n{overall}"));
    }
    let gone: Vec<String> = deleted
        .iter()
        .filter(|&&at| !deleted.iter().any(|&other| inside(sections, at, other)))
        .filter_map(|&at| sections.get(at))
        .map(|section| format!("- {}", section.path))
        .collect();
    if !gone.is_empty() {
        parts.push(format!(
            "Delete these sections with everything under them:\n{}",
            gone.join("\n")
        ));
    }
    let annotated: Vec<String> = notes
        .iter()
        .filter(|(at, note)| !note.trim().is_empty() && !deleted_at(sections, deleted, **at))
        .filter_map(|(at, note)| {
            let section = sections.get(*at)?;
            let indented: Vec<String> = note
                .trim()
                .lines()
                .map(|line| format!("  {line}"))
                .collect();
            Some(format!("- On {}:\n{}", section.path, indented.join("\n")))
        })
        .collect();
    if !annotated.is_empty() {
        parts.push(format!("Annotations:\n{}", annotated.join("\n")));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!(
        "Review of plan revision {revision}.\n\n{}",
        parts.join("\n\n")
    )
}

/// The review of one plan revision being written in its popout.
pub(in crate::app) struct ReviewDraft {
    pub(super) revision: u32,
    pub(super) deleted: BTreeSet<usize>,
    pub(super) notes: BTreeMap<usize, text_editor::Content>,
    pub(super) open_notes: BTreeSet<usize>,
    pub(super) collapsed: BTreeSet<usize>,
    pub(super) overall: text_editor::Content,
}

impl ReviewDraft {
    pub(super) fn new(revision: u32) -> ReviewDraft {
        ReviewDraft {
            revision,
            deleted: BTreeSet::new(),
            notes: BTreeMap::new(),
            open_notes: BTreeSet::new(),
            collapsed: BTreeSet::new(),
            overall: text_editor::Content::new(),
        }
    }

    /// Nothing to send: no deletion, no note with text, a blank overall.
    pub(in crate::app) fn is_empty(&self) -> bool {
        self.deleted.is_empty()
            && self
                .notes
                .values()
                .all(|note| note.text().trim().is_empty())
            && self.overall.text().trim().is_empty()
    }

    /// The review as a Refine sends it.
    pub(super) fn text(&self, sections: &[Section]) -> String {
        let notes: BTreeMap<usize, String> = self
            .notes
            .iter()
            .map(|(at, note)| (*at, note.text()))
            .collect();
        refine_text(
            sections,
            &self.deleted,
            &notes,
            &self.overall.text(),
            self.revision,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "Intro line.\n\n\
                        # Plan\n\
                        Context.\n\
                        ## Approach\n\
                        ### 1. Step\n\
                        ```sh\n\
                        # not a heading\n\
                        ```\n\
                        ### 2. Step\n\
                        text\n\
                        ## Verification\n\
                        done\n";

    fn titles(sections: &[Section]) -> Vec<(u8, &str, &str)> {
        sections
            .iter()
            .map(|s| (s.level, s.title.as_str(), s.path.as_str()))
            .collect()
    }

    #[test]
    fn a_plan_splits_at_its_headings_outside_fenced_code() {
        let found = sections(PLAN);
        assert_eq!(
            titles(&found),
            vec![
                (0, "Preamble", "Preamble"),
                (1, "Plan", "Plan"),
                (2, "Approach", "Approach"),
                (3, "1. Step", "Approach > 1. Step"),
                (3, "2. Step", "Approach > 2. Step"),
                (2, "Verification", "Verification"),
            ]
        );
        assert_eq!(&PLAN[found[0].range.clone()], "Intro line.\n\n");
        assert_eq!(
            &PLAN[found[3].range.clone()],
            "### 1. Step\n```sh\n# not a heading\n```\n"
        );
        // Approach owns both steps, up to Verification.
        let verification = PLAN.find("## Verification").unwrap();
        assert_eq!(found[2].subtree_end, verification);
        assert_eq!(found[3].subtree_end, PLAN.find("### 2.").unwrap());
        assert_eq!(found[4].subtree_end, verification);
        assert_eq!(found[1].subtree_end, PLAN.len());
        assert!(inside(&found, 3, 2) && inside(&found, 4, 1) && !inside(&found, 5, 2));
        // A blank preamble is no section; `#x` is no heading.
        let bare = sections("\n\n# T\n#x\n");
        assert_eq!(titles(&bare), vec![(1, "T", "T")]);
        // A second `# ` heading is a part of the plan, named in paths.
        let two = sections("# T\n# Part\n## Sub\n");
        assert_eq!(titles(&two)[2], (2, "Sub", "Part > Sub"));
    }

    #[test]
    fn a_refine_lists_deletions_once_and_notes_on_what_stays() {
        let found = sections(PLAN);
        let deleted = BTreeSet::from([2, 3]);
        let notes = BTreeMap::from([
            (3, "dropped with its section".to_owned()),
            (5, "Run it twice.\nThen once more.".to_owned()),
            (1, "   ".to_owned()),
        ]);
        assert_eq!(
            refine_text(&found, &deleted, &notes, " shorter \n", 2),
            "Review of plan revision 2.\n\n\
             Overall:\nshorter\n\n\
             Delete these sections with everything under them:\n\
             - Approach\n\n\
             Annotations:\n\
             - On Verification:\n  Run it twice.\n  Then once more."
        );
        assert_eq!(
            refine_text(&found, &BTreeSet::new(), &BTreeMap::new(), " ", 1),
            ""
        );
    }
}
