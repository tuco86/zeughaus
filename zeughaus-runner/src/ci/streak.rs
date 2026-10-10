//! The red streak of a repository's default branch: per job, the first
//! pipeline of its current run of failures, and the last pipeline whose
//! deploy jobs all succeeded.
//!
//! Computed from the pipeline records, so `ci status` and the scheduler see
//! the same thing and a restart forgets nothing. Push and cron pipelines of
//! the branch count; a job that a pipeline skipped or did not select says
//! nothing about it, so the streak looks past that pipeline.

use super::event::EventKind;
use super::scheduler::{JobStatus, Pipeline};

/// One job of the default branch that failed in its latest result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Red {
    pub job: String,
    /// The first failing pipeline of the current run of failures.
    pub since: u64,
    pub since_sha: Option<String>,
    /// When that pipeline was created, in seconds since the epoch.
    pub since_created: u64,
    /// Pipelines of the branch from `since` to the newest, both included.
    pub pipelines: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Streak {
    pub red: Vec<Red>,
    /// The newest pipeline in which every deploy job (one without `on`, run
    /// only because what it needs ran) succeeded, and those jobs.
    pub last_deploy: Option<(u64, Vec<String>)>,
}

/// Whether `pipeline` ran for `branch` itself: a push to it or a cron
/// schedule, which runs at its head.
pub fn is_branch(pipeline: &Pipeline, branch: &str) -> bool {
    pipeline.event.git_ref == branch
        && matches!(pipeline.event.kind, EventKind::Push | EventKind::Cron)
}

/// The streak of `branch` over `pipelines`, the records of one repository
/// in any order. Running pipelines count with the jobs that have finished.
pub fn of_branch(pipelines: &[Pipeline], branch: &str) -> Streak {
    let mut branch_pipelines: Vec<&Pipeline> = pipelines
        .iter()
        .filter(|p| is_branch(p, branch) && !p.jobs.is_empty())
        .collect();
    branch_pipelines.sort_by_key(|p| std::cmp::Reverse(p.number));

    let mut names: Vec<&str> = branch_pipelines
        .iter()
        .flat_map(|p| p.jobs.iter().map(|j| j.def.name.as_str()))
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut red = Vec::new();
    for name in names {
        let mut since = None;
        for pipeline in &branch_pipelines {
            match pipeline
                .jobs
                .iter()
                .find(|j| j.def.name == name)
                .map(|j| j.status)
            {
                Some(JobStatus::Failed) => since = Some(*pipeline),
                Some(JobStatus::Succeeded) => break,
                _ => {}
            }
        }
        if let Some(since) = since {
            red.push(Red {
                job: name.to_owned(),
                since: since.number,
                since_sha: since.sha.clone(),
                since_created: since.created,
                pipelines: branch_pipelines
                    .iter()
                    .filter(|p| p.number >= since.number)
                    .count(),
            });
        }
    }

    let last_deploy = branch_pipelines.iter().find_map(|p| {
        let deploy: Vec<_> = p.jobs.iter().filter(|j| j.def.on.is_empty()).collect();
        (!deploy.is_empty() && deploy.iter().all(|j| j.status == JobStatus::Succeeded)).then(|| {
            (
                p.number,
                deploy.iter().map(|j| j.def.name.clone()).collect(),
            )
        })
    });
    Streak { red, last_deploy }
}

/// `17 h`, `40 min`, `3 d`.
pub fn age(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 48 * 3600 => format!("{} h", s / 3600),
        s => format!("{} d", s / 86_400),
    }
}

impl Red {
    /// `windows red since #138 (79e9035, 17 h, 13 pipelines)`.
    pub fn describe(&self, now: u64) -> String {
        let sha = self.since_sha.as_deref().unwrap_or("-");
        format!(
            "{} red since #{} ({}, {}, {} pipeline{})",
            self.job,
            self.since,
            &sha[..sha.len().min(7)],
            age(now.saturating_sub(self.since_created)),
            self.pipelines,
            if self.pipelines == 1 { "" } else { "s" }
        )
    }
}

impl Streak {
    /// `windows red since #138 (...); last publish #136`, or `green`.
    pub fn describe(&self, now: u64) -> String {
        if self.red.is_empty() {
            return "green".to_owned();
        }
        let mut text = self
            .red
            .iter()
            .map(|r| r.describe(now))
            .collect::<Vec<_>>()
            .join("; ");
        if let Some((number, jobs)) = &self.last_deploy {
            text.push_str(&format!("; last {} #{number}", jobs.join(", ")));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ci::event::CiEvent;
    use crate::ci::header::{JobDef, WhenBusy};
    use crate::ci::scheduler::{JobRecord, PipelineStatus};

    fn job(name: &str, on: &[&str], status: JobStatus) -> JobRecord {
        JobRecord {
            def: JobDef {
                name: name.to_owned(),
                file: format!("{name}.sh"),
                on: on.iter().map(|s| (*s).to_owned()).collect(),
                needs: Vec::new(),
                image: None,
                machine: None,
                cache: Vec::new(),
                secrets: Vec::new(),
                when_busy: WhenBusy::Wait,
                env: Default::default(),
                timeout_minutes: 60,
                interpreter: "sh".to_owned(),
            },
            status,
            note: String::new(),
            run_dir: None,
            code: None,
            started: None,
            finished: None,
            image_tag: None,
            active_secs: 0,
            excerpt: None,
        }
    }

    fn pipeline(number: u64, git_ref: &str, windows: JobStatus, publish: JobStatus) -> Pipeline {
        Pipeline {
            repo: "griasdi".to_owned(),
            number,
            key: String::new(),
            event: CiEvent {
                repo: "griasdi".to_owned(),
                kind: EventKind::Push,
                git_ref: git_ref.to_owned(),
                sha: None,
                cron: None,
                actor: "test".to_owned(),
                delivery: None,
                received: 0,
            },
            sha: Some(format!("{number:07}abcdef")),
            status: PipelineStatus::Failed,
            note: String::new(),
            created: number * 3600,
            finished: None,
            jobs: vec![
                job("check", &["push *"], JobStatus::Succeeded),
                job("windows", &["push *"], windows),
                job("publish", &[], publish),
            ],
        }
    }

    #[test]
    fn the_streak_starts_at_the_first_failure_after_the_last_success() {
        use JobStatus::{Failed, Skipped, Succeeded};
        let pipelines = vec![
            pipeline(136, "main", Succeeded, Succeeded),
            pipeline(137, "main", Failed, Skipped),
            pipeline(138, "main", Succeeded, Succeeded),
            pipeline(139, "main", Failed, Skipped),
            // A branch's failure is not the default branch's.
            pipeline(140, "feature", Succeeded, Succeeded),
            pipeline(141, "main", Skipped, Skipped),
            pipeline(142, "main", Failed, Skipped),
        ];
        let streak = of_branch(&pipelines, "main");
        assert_eq!(streak.red.len(), 1);
        let red = &streak.red[0];
        assert_eq!(
            (red.job.as_str(), red.since, red.pipelines),
            ("windows", 139, 3)
        );
        assert_eq!(streak.last_deploy, Some((138, vec!["publish".to_owned()])));
        assert_eq!(
            streak.describe(142 * 3600 + 120),
            "windows red since #139 (0000139, 3 h, 3 pipelines); last publish #138"
        );
    }

    #[test]
    fn a_success_ends_the_streak() {
        use JobStatus::{Failed, Skipped, Succeeded};
        let pipelines = vec![
            pipeline(1, "main", Failed, Skipped),
            pipeline(2, "main", Succeeded, Succeeded),
        ];
        let streak = of_branch(&pipelines, "main");
        assert!(streak.red.is_empty());
        assert_eq!(streak.describe(0), "green");
    }
}
