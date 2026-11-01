//! How long a job is expected to run: the median of its last succeeded runs
//! at the same place for the same kind of event. A waiting agent is told when
//! a running job reaches each multiple of it; nothing is stopped by it.

use zeughaus_link::enlist::Place;

use crate::event::EventKind;
use crate::scheduler::{JobStatus, Pipeline};

/// Succeeded runs the median is taken over.
const SAMPLES: usize = 5;
/// The shortest expected time: below it every multiple would end a wait
/// within seconds.
const FLOOR_SECS: u64 = 60;

/// The kind of pipeline a run's duration is comparable within. A manual run
/// reproduces the push pipeline: `.zeughaus-ci/build.sh` picks the test tier
/// by `CI_EVENT`, nightly and tag against everything else.
fn class(kind: EventKind) -> EventKind {
    match kind {
        EventKind::Manual => EventKind::Push,
        other => other,
    }
}

/// The expected run time in seconds of job `name` on `runner` at `place` in
/// a pipeline of `kind`, from `history` (one repository's pipelines in any
/// order); `None` without a succeeded run there.
pub fn expected_secs(
    history: &[Pipeline],
    kind: EventKind,
    name: &str,
    runner: &str,
    place: &Place,
) -> Option<u64> {
    let workspace = place.workspace();
    let mut runs: Vec<(u64, u64, u64)> = history
        .iter()
        .filter(|p| class(p.event.kind) == class(kind))
        .flat_map(|p| {
            p.jobs.iter().filter_map(|job| {
                let matches = job.def.name == name
                    && job.status == JobStatus::Succeeded
                    && job.runner.as_deref() == Some(runner)
                    && job
                        .place
                        .as_ref()
                        .is_some_and(|at| at.workspace() == workspace);
                if !matches {
                    return None;
                }
                let (started, finished) = (job.started?, job.finished?);
                Some((p.created, p.number, finished.saturating_sub(started)))
            })
        })
        .collect();
    runs.sort_unstable_by_key(|&(created, number, _)| std::cmp::Reverse((created, number)));
    let mut durations: Vec<u64> = runs.iter().take(SAMPLES).map(|run| run.2).collect();
    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    Some(durations[(durations.len() - 1) / 2].max(FLOOR_SECS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::CiEvent;
    use crate::header::JobDef;
    use crate::scheduler::{JobRecord, PipelineStatus};

    /// Pipeline `number` of `kind` holding one `check` job that ran `secs`
    /// seconds on `runner` at `place` and ended with `status`.
    fn run(
        number: u64,
        kind: EventKind,
        runner: &str,
        place: Place,
        status: JobStatus,
        secs: u64,
    ) -> Pipeline {
        let started = 1_000 * number;
        Pipeline {
            repo: "demo".to_owned(),
            number,
            key: String::new(),
            event: CiEvent {
                repo: "demo".to_owned(),
                kind,
                git_ref: "main".to_owned(),
                sha: None,
                actor: "test".to_owned(),
                delivery: None,
                upload: None,
                received: 0,
            },
            sha: None,
            subject: String::new(),
            status: PipelineStatus::Succeeded,
            note: String::new(),
            created: started,
            finished: Some(started + secs),
            jobs: vec![JobRecord {
                def: JobDef {
                    name: "check".to_owned(),
                    file: "check.sh".to_owned(),
                    on: Vec::new(),
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
                code: Some(0),
                started: Some(started),
                finished: Some(started + secs),
                assigned: Some(started),
                transfers: Vec::new(),
                runner: Some(runner.to_owned()),
                place: Some(place),
                artifacts: false,
                excerpt: None,
                expected_secs: None,
                shared_with: None,
            }],
            channel: "dev".to_owned(),
            cancelled: false,
            clean: false,
        }
    }

    fn ok(number: u64, kind: EventKind, secs: u64) -> Pipeline {
        run(number, kind, "x", Place::Host, JobStatus::Succeeded, secs)
    }

    #[test]
    fn the_median_of_the_newest_five_succeeded_runs() {
        let mut history: Vec<Pipeline> = (1..=7).map(|n| ok(n, EventKind::Push, n * 100)).collect();
        history.reverse();
        assert_eq!(
            expected_secs(&history, EventKind::Push, "check", "x", &Place::Host),
            Some(500)
        );
    }

    #[test]
    fn a_manual_run_counts_as_a_push_and_a_nightly_does_not() {
        let history = vec![
            ok(1, EventKind::Manual, 200),
            ok(2, EventKind::Nightly, 900),
        ];
        assert_eq!(
            expected_secs(&history, EventKind::Push, "check", "x", &Place::Host),
            Some(200)
        );
        assert_eq!(
            expected_secs(&history, EventKind::Manual, "check", "x", &Place::Host),
            Some(200)
        );
        assert_eq!(
            expected_secs(&history, EventKind::Nightly, "check", "x", &Place::Host),
            Some(900)
        );
    }

    #[test]
    fn failed_runs_and_other_runners_and_workspaces_are_ignored() {
        let image = |tag: &str| Place::Container {
            image: "trixie".to_owned(),
            tag: tag.to_owned(),
        };
        let history = vec![
            run(1, EventKind::Push, "x", Place::Host, JobStatus::Failed, 300),
            run(
                2,
                EventKind::Push,
                "y",
                Place::Host,
                JobStatus::Succeeded,
                300,
            ),
            run(
                3,
                EventKind::Push,
                "x",
                image("a"),
                JobStatus::Succeeded,
                400,
            ),
        ];
        assert_eq!(
            expected_secs(&history, EventKind::Push, "check", "x", &Place::Host),
            None
        );
        // A new tag of the same image keeps its workspace and its history.
        assert_eq!(
            expected_secs(&history, EventKind::Push, "check", "x", &image("b")),
            Some(400)
        );
        assert_eq!(
            expected_secs(&history, EventKind::Push, "build", "x", &image("a")),
            None
        );
    }

    #[test]
    fn a_short_median_is_raised_to_a_minute() {
        let history = vec![ok(1, EventKind::Push, 10)];
        assert_eq!(
            expected_secs(&history, EventKind::Push, "check", "x", &Place::Host),
            Some(FLOOR_SECS)
        );
    }
}
