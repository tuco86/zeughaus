//! Nightly repair: a failed nightly of a repository with `nightly_repair`
//! in `ci.toml` becomes one round of tickets that run through.
//!
//! A round is one approved ticket per kind of machine its failures need,
//! each placed by a `Runs on` line: the os and arch the failed jobs ran on
//! where a session host has them, else the runner they ran on where that
//! hosts sessions, else any session host. Its master reads the failure,
//! fixes it and pushes. Jobs that failed the same way on several platforms
//! are one ticket: that failure is likely none of theirs alone. A last,
//! unplaced ticket depends on all of them and queues the nightly again,
//! whose failure files the next round.
//!
//! A round is filed only while no ticket of an earlier one is open, and at
//! most [`MAX_ROUNDS`] follow each other without a nightly that succeeded.
//! Round tickets carry the source `nightly.<pipeline>`, the last one
//! `nightly.<pipeline>.rerun`; the stored tickets are the only state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use zeughaus_link::enlist::{Offer, Place};
use zeughaus_link::selector::Selector;
use zeughaus_link::{DocState, DocView};

use crate::config::{CiConfig, arch_label};
use crate::docs;
use crate::event::EventKind;
use crate::scheduler::{JobRecord, JobStatus, Pipeline, PipelineStatus};

/// Rounds filed in a row without a nightly that succeeded, after which the
/// CI files no more.
pub const MAX_ROUNDS: usize = 3;

/// The prefix of the source of every round's tickets.
const SOURCE_PREFIX: &str = "nightly.";

/// The suffix of the source of a round's last ticket.
const RERUN_SUFFIX: &str = ".rerun";

/// A failed job's excerpt in a ticket: each line cut to this many
/// characters, and the end of the excerpt kept up to this many bytes.
const EXCERPT_LINE_CHARS: usize = 300;
const EXCERPT_BYTES: usize = 8 << 10;

/// Jobs a title names before it only counts them.
const TITLE_JOBS: usize = 3;

/// What became of a failed nightly.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No job failed (they were skipped): nothing to repair.
    Nothing,
    /// A round was filed: its tickets, then the one that runs the nightly
    /// again.
    Filed { fixes: Vec<u64>, rerun: u64 },
    /// This ticket of an earlier round is not finished.
    Open(u64),
    /// [`MAX_ROUNDS`] rounds followed each other without a nightly that
    /// succeeded.
    Exhausted,
}

/// One ticket of a round: its text and its plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub body: String,
    pub plan: String,
}

/// When the newest nightly of `pipelines` (one repository's records) that
/// succeeded finished; 0 without one.
pub fn last_green(pipelines: &[Pipeline]) -> u64 {
    pipelines
        .iter()
        .filter(|p| p.event.kind == EventKind::Nightly && p.status == PipelineStatus::Succeeded)
        .filter_map(|p| p.finished)
        .max()
        .unwrap_or(0)
}

/// The `Runs on` selector of failed `job`: `os=..,arch=..` of where it ran
/// when a session host in `hosts` has those labels, else `runner=<its
/// runner>` when that runner is a session host, else `None`, any host.
/// `offers` are the connected runners' offers; a container runs Linux and a
/// machine Windows, on their runner's arch.
fn selector_of(
    job: &JobRecord,
    offers: &BTreeMap<String, Offer>,
    hosts: &[(String, Vec<String>)],
) -> Option<String> {
    let runner = job.runner.as_deref()?;
    if let (Some(offer), Some(place)) = (offers.get(runner), &job.place) {
        let os = match place {
            Place::Host => offer.os.as_str(),
            Place::Container { .. } => "linux",
            Place::Machine { .. } => "windows",
        };
        let selector = format!("os={os},arch={}", arch_label(&offer.arch));
        if Selector::parse(&selector).is_ok_and(|s| hosts.iter().any(|(_, l)| s.matches(l))) {
            return Some(selector);
        }
    }
    hosts
        .iter()
        .any(|(name, _)| name == runner)
        .then(|| format!("runner={runner}"))
}

/// `publish (sadala, host)`.
fn job_label(job: &JobRecord) -> String {
    let runner = job.runner.as_deref().unwrap_or("no runner");
    match &job.place {
        Some(place) => format!("{} ({runner}, {})", job.def.name, place.workspace()),
        None => format!("{} ({runner})", job.def.name),
    }
}

/// The names of `jobs` for a title: all of them up to [`TITLE_JOBS`].
fn title_jobs(jobs: &[&JobRecord]) -> String {
    if jobs.len() > TITLE_JOBS {
        return format!("{} jobs failed", jobs.len());
    }
    let names: Vec<&str> = jobs.iter().map(|j| j.def.name.as_str()).collect();
    format!("{} failed", names.join(", "))
}

/// `excerpt` as an indented Markdown code block: lines cut, the end kept.
fn excerpt_block(excerpt: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0;
    for line in excerpt.lines().rev() {
        let line: String = line.chars().take(EXCERPT_LINE_CHARS).collect();
        bytes += line.len() + 5;
        if bytes > EXCERPT_BYTES {
            break;
        }
        lines.push(format!("    {line}"));
    }
    lines.reverse();
    lines.join("\n")
}

/// `#211 of zeughaus (main at a8bb627: subject)`.
fn pipeline_label(pipeline: &Pipeline) -> String {
    let mut label = format!(
        "#{} of {} ({}",
        pipeline.number, pipeline.repo, pipeline.event.git_ref
    );
    if let Some(sha) = &pipeline.sha {
        label.push_str(&format!(" at {}", &sha[..sha.len().min(7)]));
        if !pipeline.subject.is_empty() {
            label.push_str(&format!(": {}", pipeline.subject));
        }
    }
    label.push(')');
    label
}

/// `cause` with what differs between platforms for the same failure left
/// out: bracketed parts (nextest's times, rustc's error codes) and the
/// direction of path separators.
fn cause_key(cause: &str) -> String {
    let mut key = String::with_capacity(cause.len());
    let mut depth = 0u32;
    for c in cause.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            '\\' => key.push('/'),
            _ => key.push(c),
        }
    }
    key.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The failed jobs one ticket repairs.
struct Group<'a> {
    selector: Option<String>,
    jobs: Vec<&'a JobRecord>,
    /// The jobs failed the same way under several selectors.
    shared: Vec<Option<String>>,
}

/// The failed jobs of `pipeline` by the selector each needs, except that
/// jobs failing the same way under several selectors are one group, placed
/// by the first of those selectors that names one: that failure is likely
/// not one platform's, and one session fixes it.
fn groups<'a>(
    pipeline: &'a Pipeline,
    offers: &BTreeMap<String, Offer>,
    hosts: &[(String, Vec<String>)],
) -> Vec<Group<'a>> {
    let failed: Vec<(&JobRecord, Option<String>, Option<String>)> = pipeline
        .jobs
        .iter()
        .filter(|j| j.status == JobStatus::Failed)
        .map(|j| {
            (
                j,
                selector_of(j, offers, hosts),
                j.cause().map(|c| cause_key(&c)),
            )
        })
        .collect();
    let mut selectors_of: BTreeMap<&str, BTreeSet<&Option<String>>> = BTreeMap::new();
    for (_, selector, cause) in &failed {
        if let Some(cause) = cause {
            selectors_of.entry(cause).or_default().insert(selector);
        }
    }
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    enum Key<'k> {
        Placed(&'k Option<String>),
        Shared(&'k str),
    }
    let mut keyed: BTreeMap<Key<'_>, Vec<&JobRecord>> = BTreeMap::new();
    for (job, selector, cause) in &failed {
        let key = match cause.as_deref() {
            Some(cause) if selectors_of[cause].len() > 1 => Key::Shared(cause),
            _ => Key::Placed(selector),
        };
        keyed.entry(key).or_default().push(job);
    }
    keyed
        .into_iter()
        .map(|(key, jobs)| match key {
            Key::Placed(selector) => Group {
                selector: selector.clone(),
                jobs,
                shared: Vec::new(),
            },
            Key::Shared(cause) => {
                let shared: Vec<Option<String>> =
                    selectors_of[cause].iter().map(|s| (*s).clone()).collect();
                Group {
                    selector: shared.iter().flatten().next().cloned(),
                    jobs,
                    shared,
                }
            }
        })
        .collect()
}

/// The tickets that repair failed nightly `pipeline`, one per group of its
/// failed jobs (see [`groups`]); none when no job failed. A pipeline that
/// could not be set up is one ticket for any host.
pub fn fixes(
    pipeline: &Pipeline,
    offers: &BTreeMap<String, Offer>,
    hosts: &[(String, Vec<String>)],
) -> Vec<Ticket> {
    let mut groups = groups(pipeline, offers, hosts);
    if groups.is_empty() {
        if pipeline.status != PipelineStatus::Error {
            return Vec::new();
        }
        groups.push(Group {
            selector: None,
            jobs: Vec::new(),
            shared: Vec::new(),
        });
    }
    (0..groups.len())
        .map(|at| {
            let others: Vec<String> = groups
                .iter()
                .enumerate()
                .filter(|&(other, _)| other != at)
                .map(|(_, group)| {
                    let names: Vec<&str> = group.jobs.iter().map(|j| j.def.name.as_str()).collect();
                    format!(
                        "{} ({})",
                        names.join(", "),
                        group.selector.as_deref().unwrap_or("any machine")
                    )
                })
                .collect();
            fix(pipeline, &groups[at], &others)
        })
        .collect()
}

fn fix(pipeline: &Pipeline, group: &Group<'_>, others: &[String]) -> Ticket {
    let n = pipeline.number;
    let repo = &pipeline.repo;
    let branch = &pipeline.event.git_ref;
    let jobs = &group.jobs;
    let selector = group.selector.as_deref();
    let what = if jobs.is_empty() {
        "the pipeline could not be set up".to_owned()
    } else if !group.shared.is_empty() {
        format!("{} the same way", title_jobs(jobs))
    } else if let Some(selector) = selector {
        format!("{} on {selector}", title_jobs(jobs))
    } else {
        title_jobs(jobs)
    };
    let title = format!("# Nightly #{n} of {repo}: {what}");
    let names: Vec<String> = jobs.iter().map(|j| job_label(j)).collect();
    let summary = if jobs.is_empty() {
        format!(
            "The nightly pipeline {} could not be set up: {}",
            pipeline_label(pipeline),
            pipeline.note
        )
    } else {
        format!(
            "The nightly pipeline {} failed: {}.",
            pipeline_label(pipeline),
            names.join(", ")
        )
    };
    let body = format!(
        "{title}\n\n{summary} The CI filed this ticket to run through; its plan \
         says what to do.\n"
    );

    let mut plan = format!("{title}\n\n");
    if let Some(selector) = selector {
        plan.push_str(&format!("Runs on: {selector}\n\n"));
    }
    plan.push_str(&summary);
    plan.push_str(" The CI filed this ticket to repair it.");
    if !group.shared.is_empty() {
        let places: Vec<&str> = group
            .shared
            .iter()
            .map(|s| s.as_deref().unwrap_or("any machine"))
            .collect();
        plan.push_str(&format!(
            " They failed the same way on several platforms ({}), so the cause is likely \
             not one platform's: fix it once, for all of them.",
            places.join("; ")
        ));
    }
    match selector {
        _ if jobs.is_empty() => {}
        Some(s) if s.starts_with("os=") => plan.push_str(&format!(
            " This session runs on a machine with the labels {s}, where the failed jobs \
             ran, so it can build and test that platform here."
        )),
        Some(s) => plan.push_str(&format!(
            " No machine that hosts sessions has the os and arch the failed jobs ran on; \
             this session runs on their runner ({s}), so `run_jobs` reaches them from here."
        )),
        None => plan.push_str(
            " The failed jobs ran where no session runs; reproduce them with `run_jobs` \
             where it can run them.",
        ),
    }
    plan.push_str("\n\n");
    if !others.is_empty() {
        plan.push_str(&format!(
            "Other tickets of this round repair the rest of the pipeline: {}. Leave those \
             failures to them. ",
            others.join("; ")
        ));
    }
    plan.push_str(
        "A last ticket of the round queues the nightly again once every ticket of the \
         round is done.\n",
    );

    for job in jobs {
        plan.push_str(&format!("\n### {}\n\n", job.def.name));
        let mut line = job_label(job);
        if !job.note.is_empty() {
            line.push_str(&format!(": {}", job.note));
        }
        plan.push_str(&line);
        plan.push('\n');
        if let Some(cause) = job.cause() {
            let cause: String = cause.chars().take(EXCERPT_LINE_CHARS).collect();
            plan.push_str(&format!("\nCause: {cause}\n"));
        }
        if let Some(excerpt) = job.excerpt.as_deref().filter(|e| !e.trim().is_empty()) {
            plan.push_str(&format!(
                "\nThe end of its output:\n\n{}\n",
                excerpt_block(excerpt)
            ));
        }
    }

    plan.push_str("\n### Steps\n\n");
    if jobs.is_empty() {
        plan.push_str(&format!(
            "1. The note above is why the CI could not set the pipeline up \
             (`ci_pipeline` with repo `{repo}` and number {n} shows it again). A \
             `.zeughaus-ci/` the CI refuses is checked with `zeughaus-ci check` and \
             `zeughaus-ci plan .zeughaus-ci nightly {branch}` where that binary is, else \
             with `run_jobs`.\n"
        ));
    } else {
        plan.push_str(&format!(
            "1. Read each failure whole: `ci_log` with repo `{repo}`, number {n} and the \
             job; in a test run with the pattern `panicked at|FAIL \\[|error(\\[|:)` and \
             some context.\n\
             2. Find the cause and reproduce it: here when it is this platform's, else with \
             `run_jobs` of the failed job and the jobs it needs. The nightly runs tests a \
             push does not (the integration tier and slower ones): switch the check job's \
             test command temporarily to the nightly's and filter it to the failing test \
             (skill://zeughaus-ci, section 4), and undo that before committing.\n"
        ));
    }
    plan.push_str(&format!(
        "{next}. Fix the cause in the code, its tests or the job's script. Never weaken \
         a test, widen an `allow` or add a retry to get it green.\n\
         {gate}. Run the repository's gate for what you changed, commit, push to \
         {branch}, and `ci_wait` for the push pipeline of your commit.\n\
         {done}. `plan_done` with what failed, why, and what changed. A failure that is \
         not the repository's (a runner or the network away, a full disk, a forge \
         outage) gets no change: say so in `plan_done`.\n",
        next = if jobs.is_empty() { 2 } else { 3 },
        gate = if jobs.is_empty() { 3 } else { 4 },
        done = if jobs.is_empty() { 4 } else { 5 },
    ));
    Ticket { body, plan }
}

/// The round's last ticket: it waits for `fixes` and queues the nightly
/// again.
fn rerun(pipeline: &Pipeline, fixes: &[u64]) -> Ticket {
    let n = pipeline.number;
    let repo = &pipeline.repo;
    let refs: Vec<String> = fixes.iter().map(|f| format!("#{f}")).collect();
    let refs = refs.join(", ");
    let title = format!("# Nightly #{n} of {repo}: run it again");
    let body = format!(
        "{title}\n\nDepends on: {refs}\n\nThe nightly pipeline {} failed; once the \
         tickets of its repair are done this one queues the nightly again.\n",
        pipeline_label(pipeline)
    );
    let plan = format!(
        "{title}\n\n\
         The nightly pipeline {} failed, and the tickets this one depends on ({refs}) \
         repaired it. This ticket runs once they are done or dismissed; change nothing \
         in the repository.\n\n\
         ### Steps\n\n\
         1. `doc_read` each of {refs}: its report says what failed and what changed.\n\
         2. `ci_run` with repo `{repo}` and event `nightly`. Do not wait for the \
         pipeline: should it fail, the CI files the next round of repair tickets \
         itself, up to {MAX_ROUNDS} rounds in a row without a nightly that succeeded.\n\
         3. `plan_done` with the new pipeline's number and one line per ticket of the \
         round.\n",
        pipeline_label(pipeline)
    );
    Ticket { body, plan }
}

fn is_round(doc: &DocView) -> bool {
    doc.source.starts_with(SOURCE_PREFIX)
}

fn finished(state: DocState) -> bool {
    matches!(
        state,
        DocState::Done | DocState::Dismissed | DocState::Merged
    )
}

/// Files a round of repair tickets for failed nightly `pipeline`, unless a
/// ticket of an earlier round is open or [`MAX_ROUNDS`] rounds were filed
/// since `since` (when the last nightly that succeeded finished). The
/// caller holds the documents' lock.
pub fn file(
    state_dir: &Path,
    config: &CiConfig,
    pipeline: &Pipeline,
    offers: &BTreeMap<String, Offer>,
    hosts: &[(String, Vec<String>)],
    since: u64,
    now: u64,
) -> Result<Outcome, String> {
    let fixes = fixes(pipeline, offers, hosts);
    if fixes.is_empty() {
        return Ok(Outcome::Nothing);
    }
    let tickets = docs::list(state_dir, config, &pipeline.repo);
    if let Some(open) = tickets
        .iter()
        .find(|doc| is_round(doc) && !finished(doc.state))
    {
        return Ok(Outcome::Open(open.number));
    }
    let rounds = tickets
        .iter()
        .filter(|doc| is_round(doc) && doc.source.ends_with(RERUN_SUFFIX) && doc.created > since)
        .count();
    if rounds >= MAX_ROUNDS {
        return Ok(Outcome::Exhausted);
    }
    let source = format!("{SOURCE_PREFIX}{}", pipeline.number);
    let mut numbers = Vec::with_capacity(fixes.len());
    for ticket in &fixes {
        numbers.push(docs::file_approved(
            state_dir,
            config,
            &pipeline.repo,
            &ticket.body,
            &ticket.plan,
            &source,
            now,
        )?);
    }
    let last = rerun(pipeline, &numbers);
    let rerun = docs::file_approved(
        state_dir,
        config,
        &pipeline.repo,
        &last.body,
        &last.plan,
        &format!("{source}{RERUN_SUFFIX}"),
        now,
    )?;
    Ok(Outcome::Filed {
        fixes: numbers,
        rerun,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CiEvent;
    use crate::header::JobDef;
    use zeughaus_link::ci::runs_on_named;

    fn job(name: &str, runner: &str, place: Place, status: JobStatus) -> JobRecord {
        JobRecord {
            def: JobDef {
                name: name.to_owned(),
                file: format!("{name}.sh"),
                on: vec!["nightly".to_owned()],
                needs: Vec::new(),
                image: None,
                machine: None,
                platform: None,
                ignored_keys: Vec::new(),
                channels: Vec::new(),
                secrets: Vec::new(),
                env: Default::default(),
                timeout_minutes: 60,
                interpreter: "sh".to_owned(),
                state: Vec::new(),
                state_save: zeughaus_link::enlist::StateSave::Success,
            },
            status,
            note: String::new(),
            code: None,
            started: None,
            assigned: None,
            transfers: Vec::new(),
            finished: None,
            runner: Some(runner.to_owned()),
            place: Some(place),
            artifacts: false,
            excerpt: None,
            expected_secs: None,
            shared_with: None,
        }
    }

    fn container() -> Place {
        Place::Container {
            image: "trixie".to_owned(),
            tag: "t".to_owned(),
        }
    }

    fn pipeline(status: PipelineStatus, jobs: Vec<JobRecord>) -> Pipeline {
        Pipeline {
            repo: "demo".to_owned(),
            number: 211,
            key: String::new(),
            event: CiEvent {
                repo: "demo".to_owned(),
                kind: EventKind::Nightly,
                git_ref: "main".to_owned(),
                sha: None,
                actor: "nightly".to_owned(),
                delivery: None,
                upload: None,
                upstream: None,
                received: 0,
            },
            sha: Some("a8bb6270000".to_owned()),
            subject: "docs: a change".to_owned(),
            status,
            note: String::new(),
            created: 0,
            finished: Some(10),
            jobs,
            channel: "nightly".to_owned(),
            cancelled: false,
            clean: false,
        }
    }

    fn offer(os: &str, arch: &str) -> Offer {
        Offer {
            os: os.to_owned(),
            arch: arch.to_owned(),
            cpus: 8,
            containers: true,
            machines: Vec::new(),
        }
    }

    /// Connected runners' offers and the session hosts with their labels.
    type Fleet = (BTreeMap<String, Offer>, Vec<(String, Vec<String>)>);

    /// The runners of the user's CI: a Linux amd64 service runner and a
    /// Linux arm64 server that host no sessions, a Linux amd64 desktop, a
    /// Mac and a Windows PC that do.
    fn fleet() -> Fleet {
        let offers: BTreeMap<String, Offer> = [
            ("service", offer("linux", "x86_64")),
            ("server", offer("linux", "aarch64")),
            ("desktop", offer("linux", "x86_64")),
            ("mac", offer("macos", "aarch64")),
            ("pc", offer("windows", "x86_64")),
        ]
        .into_iter()
        .map(|(name, offer)| (name.to_owned(), offer))
        .collect();
        let labels = |name: &str, os: &str, arch: &str| {
            (
                name.to_owned(),
                vec![
                    format!("arch={arch}"),
                    format!("os={os}"),
                    format!("runner={name}"),
                ],
            )
        };
        let hosts = vec![
            labels("desktop", "linux", "amd64"),
            labels("mac", "macos", "arm64"),
            labels("pc", "windows", "amd64"),
        ];
        (offers, hosts)
    }

    fn placed(tickets: &[Ticket]) -> Vec<Vec<String>> {
        tickets
            .iter()
            .map(|t| runs_on_named(&t.plan).expect("a valid Runs on line"))
            .collect()
    }

    #[test]
    fn failed_jobs_are_grouped_by_the_platform_a_session_host_has() {
        let (offers, hosts) = fleet();
        let mut check = job(
            "check-linux-amd64",
            "service",
            container(),
            JobStatus::Failed,
        );
        check.excerpt =
            Some("running 3 tests\nthread 'x' panicked at src/a.rs:1:1:\nboom".to_owned());
        let p = pipeline(
            PipelineStatus::Failed,
            vec![
                check,
                job("lint", "service", Place::Host, JobStatus::Failed),
                job("check-darwin-arm64", "mac", Place::Host, JobStatus::Failed),
                job(
                    "check-windows",
                    "service",
                    Place::Machine {
                        name: "win11".to_owned(),
                    },
                    JobStatus::Failed,
                ),
                // arm64 Linux: no host has it, and its runner hosts sessions.
                job("check-linux-arm64", "mac", container(), JobStatus::Failed),
                // Its runner hosts none: any host.
                job("publish", "server", Place::Host, JobStatus::Failed),
                job("build", "service", Place::Host, JobStatus::Succeeded),
                job("skipped", "pc", Place::Host, JobStatus::Skipped),
            ],
        );
        let tickets = fixes(&p, &offers, &hosts);
        assert_eq!(
            placed(&tickets),
            [
                vec![],
                vec!["os=linux,arch=amd64".to_owned()],
                vec!["os=macos,arch=arm64".to_owned()],
                vec!["os=windows,arch=amd64".to_owned()],
                vec!["runner=mac".to_owned()],
            ]
        );
        let linux = &tickets[1];
        assert!(
            linux.plan.starts_with(
                "# Nightly #211 of demo: check-linux-amd64, lint failed on os=linux,arch=amd64\n"
            ),
            "{}",
            linux.plan
        );
        assert!(
            linux
                .plan
                .contains("\nCause: thread 'x' panicked at src/a.rs:1:1: boom\n")
        );
        assert!(linux.plan.contains("\n    boom\n"));
        assert!(
            linux.plan.contains("publish (any machine)"),
            "{}",
            linux.plan
        );
        assert!(!linux.plan.contains("### build"));
        assert!(linux.body.starts_with("# Nightly #211 of demo: "));
        assert!(!linux.body.contains("Runs on"));
    }

    #[test]
    fn a_failure_shared_by_platforms_is_one_ticket() {
        let (offers, hosts) = fleet();
        let failing = |name: &str, runner: &str, place: Place, excerpt: &str| {
            let mut job = job(name, runner, place, JobStatus::Failed);
            job.excerpt = Some(excerpt.to_owned());
            job
        };
        let p = pipeline(
            PipelineStatus::Failed,
            vec![
                failing(
                    "check-linux",
                    "service",
                    container(),
                    "        FAIL [   0.412s] server::it swarm::whole\nerror: test run failed",
                ),
                failing(
                    "check-windows",
                    "pc",
                    Place::Host,
                    "        FAIL [   1.020s] server::it swarm::whole\r\nerror: test run failed",
                ),
                failing(
                    "check-mac",
                    "mac",
                    Place::Host,
                    "error: linker `cc` not found",
                ),
            ],
        );
        let tickets = fixes(&p, &offers, &hosts);
        assert_eq!(
            placed(&tickets),
            [
                vec!["os=macos,arch=arm64".to_owned()],
                vec!["os=linux,arch=amd64".to_owned()],
            ]
        );
        assert!(
            tickets[1].plan.starts_with(
                "# Nightly #211 of demo: check-linux, check-windows failed the same way\n"
            ),
            "{}",
            tickets[1].plan
        );
        assert!(
            tickets[1]
                .plan
                .contains("(os=linux,arch=amd64; os=windows,arch=amd64)")
        );
    }

    #[test]
    fn a_runner_that_is_away_leaves_its_jobs_to_any_host() {
        let (mut offers, hosts) = fleet();
        offers.remove("service");
        let p = pipeline(
            PipelineStatus::Failed,
            vec![job("check", "service", Place::Host, JobStatus::Failed)],
        );
        assert_eq!(placed(&fixes(&p, &offers, &hosts)), [Vec::<String>::new()]);
    }

    #[test]
    fn only_failed_jobs_or_a_failed_setup_are_repaired() {
        let (offers, hosts) = fleet();
        let skipped = pipeline(
            PipelineStatus::Failed,
            vec![job("check", "pc", Place::Host, JobStatus::Skipped)],
        );
        assert!(fixes(&skipped, &offers, &hosts).is_empty());

        let mut setup = pipeline(PipelineStatus::Error, Vec::new());
        setup.note = "channel `nightly`: unknown pattern kind `cron`".to_owned();
        let tickets = fixes(&setup, &offers, &hosts);
        assert_eq!(placed(&tickets), [Vec::<String>::new()]);
        assert!(
            tickets[0]
                .plan
                .contains("could not be set up: channel `nightly`: unknown pattern kind `cron`"),
            "{}",
            tickets[0].plan
        );
    }

    #[test]
    fn an_excerpt_keeps_its_end_within_bounds() {
        let long = "x".repeat(1000);
        let excerpt: String = (0..100).map(|i| format!("{i} {long}\n")).collect();
        let block = excerpt_block(&excerpt);
        assert!(block.len() <= EXCERPT_BYTES);
        assert!(block.ends_with(&format!("    99 {}", "x".repeat(EXCERPT_LINE_CHARS - 3))));
        assert!(block.lines().all(|l| l.starts_with("    ")));
    }

    #[test]
    fn the_last_green_nightly_is_the_newest_that_succeeded() {
        let at = |kind, status, finished| {
            let mut p = pipeline(status, Vec::new());
            p.event.kind = kind;
            p.finished = Some(finished);
            p
        };
        assert_eq!(last_green(&[]), 0);
        assert_eq!(
            last_green(&[
                at(EventKind::Nightly, PipelineStatus::Succeeded, 5),
                at(EventKind::Nightly, PipelineStatus::Succeeded, 9),
                at(EventKind::Nightly, PipelineStatus::Failed, 12),
                at(EventKind::Push, PipelineStatus::Succeeded, 20),
            ]),
            9
        );
    }

    /// Tests that touch the filesystem: the integration tier.
    mod io {
        use super::*;
        use zeughaus_link::{DocLink, LinkKind};

        #[test]
        fn rounds_follow_each_other_until_a_nightly_succeeds() {
            let state = std::env::temp_dir().join(format!("zci-repair-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&state);
            std::fs::create_dir_all(&state).expect("create the scratch directory");
            let config = CiConfig::parse(
                "default_runner = \"x\"\n\
                 [runners.x]\nfingerprint = \"sha256:0000000000000000000000000000000000000000000000000000000000000001\"\n\
                 [repos.demo]\nurl = \"https://git.example.org/o/demo.git\"\nnightly_repair = true\n",
            )
            .expect("a valid ci.toml");
            let (offers, hosts) = fleet();
            let failed = pipeline(
                PipelineStatus::Failed,
                vec![
                    job("check-linux", "desktop", Place::Host, JobStatus::Failed),
                    job("check-mac", "mac", Place::Host, JobStatus::Failed),
                ],
            );
            let file = |since, now| file(&state, &config, &failed, &offers, &hosts, since, now);

            assert_eq!(
                file(0, 100),
                Ok(Outcome::Filed {
                    fixes: vec![1, 2],
                    rerun: 3
                })
            );
            let load = |n| docs::load(&state, &config, "demo", n).expect("stored");
            let (linux, rerun) = (load(1), load(3));
            assert_eq!(
                (linux.state, linux.source.as_str(), linux.runs_on.clone()),
                (
                    DocState::Approved,
                    "nightly.211",
                    vec!["os=linux,arch=amd64".to_owned()]
                )
            );
            assert!(linux.plan_text.contains("### check-linux"));
            assert_eq!(
                (rerun.state, rerun.source.as_str(), rerun.runs_on.len()),
                (DocState::Approved, "nightly.211.rerun", 0)
            );
            let depends = |n| DocLink {
                kind: LinkKind::DependsOn,
                repo: None,
                number: n,
            };
            assert_eq!(rerun.links, [depends(1), depends(2)]);
            assert_eq!(rerun.waits_for, [depends(1), depends(2)]);

            // A failure while the round is open files nothing.
            assert_eq!(file(0, 200), Ok(Outcome::Open(1)));

            let close = |numbers: &[u64], now| {
                for &n in numbers {
                    docs::set_state(&state, &config, "demo", n, DocState::Dismissed, now)
                        .expect("dismiss");
                }
            };
            close(&[1, 2, 3], 300);
            assert!(matches!(file(0, 400), Ok(Outcome::Filed { rerun: 6, .. })));
            close(&[4, 5, 6], 500);
            assert!(matches!(file(0, 600), Ok(Outcome::Filed { rerun: 9, .. })));
            close(&[7, 8, 9], 700);
            assert_eq!(file(0, 800), Ok(Outcome::Exhausted));
            // A nightly that succeeded after the third round starts the count over.
            assert!(matches!(
                file(650, 900),
                Ok(Outcome::Filed { rerun: 12, .. })
            ));
            let _ = std::fs::remove_dir_all(&state);
        }
    }
}
