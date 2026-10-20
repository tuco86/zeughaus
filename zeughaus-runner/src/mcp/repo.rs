//! A checkout, and what the CI says of the repository it belongs to.
//!
//! The CI finds a repository by the `origin` of a checkout, the way a manual
//! run names it, so an agent in a checkout never has to know the
//! repository's name. [`describe`] is the text an agent starts a session
//! with: which repository this is, whether its default branch and the
//! checkout's `HEAD` are green, and every job of its `.zeughaus-ci/` with
//! the runner and place it runs at and whether `run_jobs` can run it.

use std::path::{Path, PathBuf};

use zeughaus_link::{CiReply, CiRequest, DocKind, DocState, JobDefView, RepoView};

use super::client::Client;
use super::snapshot::{git, toplevel, without_credentials};
use super::tools::{one_line, short, unexpected};

/// A git checkout, as far as the CI needs to know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    /// The top level of its working tree.
    pub root: PathBuf,
    /// The `origin` remote without credentials: what names the repository to
    /// the CI.
    pub origin: String,
    /// The commit `HEAD` names; `None` while the checkout has no commit.
    pub head: Option<String>,
}

/// The checkout `path` is in.
///
/// Blocking: runs git. Call it on the blocking pool.
pub fn checkout(path: &Path) -> Result<Checkout, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let root = toplevel(path)?;
    let origin = git(&root, &[], &["remote", "get-url", "origin"], None)
        .map(|url| without_credentials(url.trim()))
        .ok()
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            format!(
                "{} has no origin remote, so no CI repository can be named",
                root.display()
            )
        })?;
    // `--verify --quiet`: a checkout without a commit is an answer, not a
    // failure with git's complaint in it.
    let head = git(
        &root,
        &[],
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        None,
    )
    .ok()
    .map(|sha| sha.trim().to_owned())
    .filter(|sha| !sha.is_empty());
    Ok(Checkout { root, origin, head })
}

/// Asks the CI which of its repositories `checkout` is. With `jobs` the CI
/// also reads the jobs of the repository's `.zeughaus-ci/`, which costs it
/// git reads: a caller that only needs the name leaves it off. `None` means
/// that no repository of the CI's `ci.toml` has this origin.
pub async fn ask(
    client: &Client,
    checkout: &Checkout,
    jobs: bool,
) -> Result<Option<RepoView>, String> {
    let request = CiRequest::Repo {
        origin: checkout.origin.clone(),
        sha: checkout.head.clone(),
        jobs,
    };
    match client.ci(request).await? {
        CiReply::Repo { repo } => Ok(repo.map(|repo| *repo)),
        other => Err(unexpected(&other)),
    }
}

/// The repository as an agent is told of it, one fact per line and without a
/// final newline. `head` is the checkout's `HEAD`.
///
/// ```text
/// zeughaus (forgejo tuco86/zeughaus): main green
/// HEAD 3635dad: dev #64 succeeded
/// channels: dev #64 succeeded, nightly #48 succeeded
/// jobs at 3635dad (run_jobs takes these names):
///   build-linux-amd64  tuco-pc image:trixie  on push main, tag v*
///   check-linux-amd64  tuco-pc image:trixie  needs build-linux-amd64
///   publish            sadala host           needs build-linux-amd64; secrets KEY (none in run_jobs)
/// runners: atik online, tuco-pc free; machines: win11 off
/// platforms: linux-amd64 -> tuco-pc
/// ```
///
/// The jobs are columns: name, `runner at` and then what triggers the job
/// (`on`, or `needs`), the secrets it is given and why `run_jobs` cannot run
/// it. A job the CI cannot place says why instead of the runner.
pub fn describe(view: &RepoView, head: Option<&str>) -> String {
    let mut lines = vec![
        format!(
            "{} ({}): {} {}",
            view.name,
            view.forge,
            view.default_branch,
            one_line(&view.branch)
        ),
        head_line(view, head),
        format!(
            "channels: {}",
            listed(view.channels.iter().map(|channel| format!(
                "{} #{} {}",
                channel.channel, channel.latest, channel.status
            )))
        ),
    ];
    lines.extend(job_lines(view));
    lines.push(runners_line(view));
    lines.push(format!(
        "platforms: {}",
        listed(
            view.platforms
                .iter()
                .map(|(platform, target)| format!("{platform} -> {target}"))
        )
    ));
    lines.extend(doc_lines(view));
    lines.join("\n")
}

/// The open proposals and the plans that wait or run, each line left out
/// when it has none.
fn doc_lines(view: &RepoView) -> Vec<String> {
    let proposals: Vec<String> = view
        .docs
        .iter()
        .filter(|doc| doc.kind == DocKind::Proposal && doc.state == DocState::Open)
        .map(|doc| format!("#{} {} (from {})", doc.number, doc.title, doc.source))
        .collect();
    let plans: Vec<String> = view
        .docs
        .iter()
        .filter(|doc| doc.kind == DocKind::Plan)
        .map(|doc| format!("#{} {} {}", doc.number, doc.state.name(), doc.title))
        .collect();
    let mut lines = Vec::new();
    if !proposals.is_empty() {
        lines.push(format!("Open proposals: {}", proposals.join(", ")));
    }
    if !plans.is_empty() {
        lines.push(format!("Plans: {}", plans.join(", ")));
    }
    lines
}

/// `items` joined by commas, or `none`.
fn listed(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    if items.is_empty() {
        "none".to_owned()
    } else {
        items.join(", ")
    }
}

/// The newest pipeline of the checkout's commit. A failed one names its
/// first failed job and why it failed; one that never ran its jobs (an
/// error, a cancellation) says why with the pipeline's note.
fn head_line(view: &RepoView, head: Option<&str>) -> String {
    let Some(head) = head else {
        return "HEAD: no commit".to_owned();
    };
    let sha = short(Some(head));
    let Some(pipeline) = view.head.as_deref() else {
        return format!("HEAD {sha}: no pipeline (not pushed, or no job selected)");
    };
    let mut line = format!(
        "HEAD {sha}: {} #{} {}",
        pipeline.channel, pipeline.number, pipeline.status
    );
    match pipeline.status.as_str() {
        "running" | "succeeded" => {}
        "failed" => {
            if let Some(job) = pipeline.jobs.iter().find(|job| job.status == "failed") {
                line.push_str(&format!(": {} failed", job.name));
                let cause = job.cause.as_deref().map(one_line).unwrap_or_default();
                if !cause.is_empty() {
                    line.push_str(&format!(": {cause}"));
                }
            }
        }
        _ => {
            let note = one_line(&pipeline.note);
            if !note.is_empty() {
                line.push_str(&format!(": {note}"));
            }
        }
    }
    line
}

/// The jobs block, or the one line that says why there is none.
fn job_lines(view: &RepoView) -> Vec<String> {
    if view.jobs.is_empty() {
        let note = one_line(&view.jobs_note);
        let note = if note.is_empty() { "none" } else { &note };
        return vec![format!("jobs: {note}")];
    }
    let mut lines = vec![match view.jobs_sha.as_deref() {
        Some(sha) => format!("jobs at {} (run_jobs takes these names):", short(Some(sha))),
        None => "jobs (run_jobs takes these names):".to_owned(),
    }];
    let places: Vec<String> = view.jobs.iter().map(place_of).collect();
    let name_width = view
        .jobs
        .iter()
        .map(|job| job.name.chars().count())
        .max()
        .unwrap_or(0);
    // A job that cannot be placed has a sentence for a place, which must not
    // push the others' notes out of line.
    let place_width = view
        .jobs
        .iter()
        .zip(&places)
        .filter(|(job, _)| job.placement.is_empty())
        .map(|(_, place)| place.chars().count())
        .max()
        .unwrap_or(0);
    for (job, place) in view.jobs.iter().zip(&places) {
        let line = format!(
            "  {:<name_width$}  {place:<place_width$}  {}",
            job.name,
            notes_of(job)
        );
        lines.push(line.trim_end().to_owned());
    }
    lines
}

/// `<runner> <at>`, or why the job has no runner.
fn place_of(job: &JobDefView) -> String {
    if job.placement.is_empty() {
        format!("{} {}", job.runner, job.at)
    } else {
        format!("cannot be placed: {}", one_line(&job.placement))
    }
}

/// What starts the job, what it is given and what keeps `run_jobs` from
/// running it, separated by semicolons.
fn notes_of(job: &JobDefView) -> String {
    let mut parts = Vec::new();
    if !job.on.is_empty() {
        parts.push(format!("on {}", job.on.join(", ")));
    }
    if !job.needs.is_empty() {
        parts.push(format!("needs {}", job.needs.join(", ")));
    }
    if !job.channels.is_empty() {
        parts.push(format!("channels {}", job.channels.join(", ")));
    }
    if !job.secrets.is_empty() {
        parts.push(format!(
            "secrets {} (none in run_jobs)",
            job.secrets.join(", ")
        ));
    }
    // An unplaced job already says it has no place.
    if !job.manual && job.placement.is_empty() {
        parts.push(
            if job.at.starts_with("machine:") {
                "run_jobs cannot run it (Windows VM)"
            } else {
                "run_jobs cannot run it"
            }
            .to_owned(),
        );
    }
    parts.join("; ")
}

/// The runners and their state, then the machines (the Windows VMs a runner
/// boots) when there are any.
fn runners_line(view: &RepoView) -> String {
    let named = |vm: bool| {
        view.machines
            .iter()
            .filter(move |machine| (machine.kind == "windows-vm") == vm)
            .map(|machine| format!("{} {}", machine.name, machine.status))
    };
    let mut line = format!("runners: {}", listed(named(false)));
    let machines: Vec<String> = named(true).collect();
    if !machines.is_empty() {
        line.push_str(&format!("; machines: {}", machines.join(", ")));
    }
    line
}

#[cfg(test)]
mod tests {
    use zeughaus_link::{ChannelView, JobView, MachineView, PipelineView};

    use super::*;

    const HEAD: &str = "3635dad0123456789abcdef0123456789abcdef0";

    fn job_def(name: &str, on: &[&str], needs: &[&str], runner: &str, at: &str) -> JobDefView {
        let owned = |list: &[&str]| list.iter().map(|item| (*item).to_owned()).collect();
        JobDefView {
            name: name.to_owned(),
            file: format!("{name}.sh"),
            on: owned(on),
            needs: owned(needs),
            channels: Vec::new(),
            place: at.to_owned(),
            runner: runner.to_owned(),
            at: at.to_owned(),
            placement: String::new(),
            secrets: Vec::new(),
            when_busy: "wait".to_owned(),
            manual: !at.starts_with("machine:"),
        }
    }

    fn job_view(name: &str, status: &str, cause: Option<&str>) -> JobView {
        JobView {
            name: name.to_owned(),
            needs: Vec::new(),
            place: "tuco-pc".to_owned(),
            status: status.to_owned(),
            note: String::new(),
            started: None,
            finished: None,
            code: None,
            cause: cause.map(str::to_owned),
            excerpt: None,
            assigned: None,
            transfers: Vec::new(),
        }
    }

    fn pipeline_view(number: u64, status: &str, jobs: Vec<JobView>) -> PipelineView {
        PipelineView {
            repo: "zeughaus".to_owned(),
            number,
            channel: "dev".to_owned(),
            event: "push".to_owned(),
            git_ref: "main".to_owned(),
            sha: Some(HEAD.to_owned()),
            actor: "hannes".to_owned(),
            subject: "feat: x".to_owned(),
            status: status.to_owned(),
            note: String::new(),
            created: 1_000,
            finished: None,
            jobs,
        }
    }

    fn machine(name: &str, kind: &str, status: &str) -> MachineView {
        MachineView {
            name: name.to_owned(),
            kind: kind.to_owned(),
            status: status.to_owned(),
            jobs: Vec::new(),
        }
    }

    /// The repository of the CI's own checkout, in the state the reference
    /// documentation shows.
    fn zeughaus() -> RepoView {
        let channel = |channel: &str, latest| ChannelView {
            repo: "zeughaus".to_owned(),
            channel: channel.to_owned(),
            latest,
            status: "succeeded".to_owned(),
        };
        let mut publish = job_def(
            "publish",
            &[],
            &[
                "build-linux-amd64",
                "build-windows-amd64",
                "check-linux-amd64",
                "check-windows-amd64",
            ],
            "sadala",
            "host",
        );
        publish.secrets = vec![
            "ZEUGHAUS_UPDATE_KEY".to_owned(),
            "ZEUGHAUS_GITHUB_TOKEN".to_owned(),
        ];
        RepoView {
            name: "zeughaus".to_owned(),
            forge: "forgejo tuco86/zeughaus".to_owned(),
            default_branch: "main".to_owned(),
            branch: "green".to_owned(),
            channels: vec![
                channel("dev", 64),
                channel("nightly", 48),
                channel("release", 37),
            ],
            head: Some(Box::new(pipeline_view(
                64,
                "succeeded",
                vec![job_view("build-linux-amd64", "succeeded", None)],
            ))),
            jobs_sha: Some(HEAD.to_owned()),
            jobs: vec![
                job_def(
                    "build-linux-amd64",
                    &["push main", "tag v*", "cron 0 3 * * *"],
                    &[],
                    "tuco-pc",
                    "image:trixie",
                ),
                job_def(
                    "check-linux-amd64",
                    &[],
                    &["build-linux-amd64"],
                    "tuco-pc",
                    "image:trixie",
                ),
                job_def(
                    "check-windows-amd64",
                    &[],
                    &["build-windows-amd64"],
                    "wohnzimmer",
                    "host",
                ),
                publish,
                job_def(
                    "windows-amd64",
                    &["push main"],
                    &[],
                    "tuco-pc",
                    "machine:win11",
                ),
            ],
            jobs_note: String::new(),
            machines: vec![
                machine("atik", "runner", "online"),
                machine("sadala", "runner", "online"),
                machine("tuco-pc", "runner", "free"),
                machine("tuco-pc-hannes", "runner", "online"),
                machine("wohnzimmer", "runner", "online"),
                machine("win11", "windows-vm", "off"),
            ],
            platforms: vec![
                ("darwin-arm64".to_owned(), "atik".to_owned()),
                ("linux-amd64".to_owned(), "tuco-pc".to_owned()),
                ("linux-arm64".to_owned(), "atik".to_owned()),
                ("windows-amd64".to_owned(), "win11".to_owned()),
            ],
            docs: Vec::new(),
        }
    }

    /// What the agent reads at the start of a session in a green checkout:
    /// the consumers quote these lines, so they are pinned whole.
    #[test]
    fn a_green_checkout_is_told_with_its_jobs_where_they_run_and_what_run_jobs_cannot_do() {
        assert_eq!(
            describe(&zeughaus(), Some(HEAD)),
            concat!(
                "zeughaus (forgejo tuco86/zeughaus): main green\n",
                "HEAD 3635dad: dev #64 succeeded\n",
                "channels: dev #64 succeeded, nightly #48 succeeded, release #37 succeeded\n",
                "jobs at 3635dad (run_jobs takes these names):\n",
                "  build-linux-amd64    tuco-pc image:trixie   on push main, tag v*, cron 0 3 * * *\n",
                "  check-linux-amd64    tuco-pc image:trixie   needs build-linux-amd64\n",
                "  check-windows-amd64  wohnzimmer host        needs build-windows-amd64\n",
                "  publish              sadala host            needs build-linux-amd64, build-windows-amd64, check-linux-amd64, check-windows-amd64; secrets ZEUGHAUS_UPDATE_KEY, ZEUGHAUS_GITHUB_TOKEN (none in run_jobs)\n",
                "  windows-amd64        tuco-pc machine:win11  on push main; run_jobs cannot run it (Windows VM)\n",
                "runners: atik online, sadala online, tuco-pc free, tuco-pc-hannes online, wohnzimmer online; machines: win11 off\n",
                "platforms: darwin-arm64 -> atik, linux-amd64 -> tuco-pc, linux-arm64 -> atik, windows-amd64 -> win11",
            )
        );
    }

    #[test]
    fn open_proposals_and_waiting_plans_close_the_description() {
        let doc = |number, kind, state, title: &str| zeughaus_link::DocView {
            repo: "zeughaus".to_owned(),
            number,
            kind,
            title: title.to_owned(),
            state,
            source: "weida".to_owned(),
            created: 0,
            changed: 0,
            approved: None,
            proposals: Vec::new(),
            plan: None,
            body: String::new(),
        };
        let mut view = zeughaus();
        view.docs = vec![
            doc(1, DocKind::Plan, DocState::Approved, "Store plans"),
            doc(3, DocKind::Proposal, DocState::Open, "Expose the clock"),
            doc(4, DocKind::Plan, DocState::Saved, "Plans tab"),
        ];
        let text = describe(&view, Some(HEAD));
        assert!(
            text.ends_with(
                "\nOpen proposals: #3 Expose the clock (from weida)\n\
                 Plans: #1 approved Store plans, #4 saved Plans tab"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_red_head_names_the_first_job_that_failed_and_why() {
        let mut view = zeughaus();
        view.branch = "red since #65".to_owned();
        view.head = Some(Box::new(pipeline_view(
            65,
            "failed",
            vec![
                job_view("build-linux-amd64", "succeeded", None),
                job_view(
                    "check-linux-amd64",
                    "failed",
                    Some("error[E0425]:  cannot find value `x`\n  in this scope"),
                ),
                job_view("check-windows-amd64", "failed", Some("later")),
            ],
        )));
        let text = describe(&view, Some(HEAD));
        let mut lines = text.lines();
        assert_eq!(
            lines.next(),
            Some("zeughaus (forgejo tuco86/zeughaus): main red since #65")
        );
        assert_eq!(
            lines.next(),
            Some(
                "HEAD 3635dad: dev #65 failed: check-linux-amd64 failed: error[E0425]: \
                 cannot find value `x` in this scope"
            )
        );

        // A failed job that has no cause line is still named, and a failed
        // pipeline none of whose jobs failed is only called failed.
        let failed = |jobs: Vec<JobView>| {
            let mut view = zeughaus();
            view.head = Some(Box::new(pipeline_view(66, "failed", jobs)));
            describe(&view, Some(HEAD))
                .lines()
                .nth(1)
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            failed(vec![job_view("check-linux-amd64", "failed", None)]),
            "HEAD 3635dad: dev #66 failed: check-linux-amd64 failed"
        );
        assert_eq!(
            failed(vec![job_view("check-linux-amd64", "skipped", None)]),
            "HEAD 3635dad: dev #66 failed"
        );
    }

    #[test]
    fn a_head_that_has_no_pipeline_or_no_commit_says_so() {
        let mut view = zeughaus();
        view.head = None;
        assert_eq!(
            describe(&view, Some(HEAD)).lines().nth(1),
            Some("HEAD 3635dad: no pipeline (not pushed, or no job selected)")
        );
        // Without a commit there is nothing to have a pipeline of, whatever
        // the CI answered.
        view.head = Some(Box::new(pipeline_view(64, "succeeded", Vec::new())));
        assert_eq!(
            describe(&view, None).lines().nth(1),
            Some("HEAD: no commit")
        );
    }

    #[test]
    fn a_pipeline_that_never_ran_says_why_and_one_that_runs_does_not() {
        let head_line = |status: &str, note: &str| {
            let mut view = zeughaus();
            let mut pipeline = pipeline_view(67, status, Vec::new());
            pipeline.note = note.to_owned();
            view.head = Some(Box::new(pipeline));
            describe(&view, Some(HEAD))
                .lines()
                .nth(1)
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            head_line("error", "check.sh: unknown key\nwhen_busy"),
            "HEAD 3635dad: dev #67 error: check.sh: unknown key when_busy"
        );
        assert_eq!(
            head_line("no-jobs", "no .zeughaus-ci/ in this commit"),
            "HEAD 3635dad: dev #67 no-jobs: no .zeughaus-ci/ in this commit"
        );
        assert_eq!(
            head_line("cancelled", ""),
            "HEAD 3635dad: dev #67 cancelled"
        );
        assert_eq!(
            head_line("running", "a note a running pipeline may carry"),
            "HEAD 3635dad: dev #67 running"
        );
    }

    #[test]
    fn a_job_that_cannot_be_placed_says_why_in_place_of_its_runner_and_keeps_the_others_aligned() {
        let mut view = zeughaus();
        let mut lost = job_def("lost", &["push main"], &[], "", "");
        lost.placement = "machine nowhere is no runner or machine of ci.toml".to_owned();
        lost.manual = false;
        lost.secrets = vec!["TOKEN".to_owned()];
        view.jobs = vec![
            job_def("a", &["push main"], &[], "tuco-pc", "host"),
            lost,
            job_def("bb", &[], &["a"], "sadala", "image:x"),
        ];
        view.jobs_sha = Some("abcdef0123".to_owned());
        let text = describe(&view, Some(HEAD));
        let jobs: Vec<&str> = text.lines().skip(3).take(4).collect();
        assert_eq!(
            jobs,
            [
                "jobs at abcdef0 (run_jobs takes these names):",
                "  a     tuco-pc host    on push main",
                "  lost  cannot be placed: machine nowhere is no runner or machine of ci.toml  on push main; secrets TOKEN (none in run_jobs)",
                "  bb    sadala image:x  needs a",
            ]
        );
    }

    #[test]
    fn a_repository_without_readable_jobs_gives_the_reason_and_nothing_to_list() {
        let mut view = zeughaus();
        view.jobs.clear();
        view.jobs_sha = None;
        view.jobs_note =
            "the CI has not fetched zeughaus yet;\nit does with its first event".to_owned();
        view.channels.clear();
        view.machines = vec![machine("atik", "runner", "offline")];
        view.platforms.clear();
        view.forge = "none".to_owned();
        assert_eq!(
            describe(&view, Some(HEAD)),
            concat!(
                "zeughaus (none): main green\n",
                "HEAD 3635dad: dev #64 succeeded\n",
                "channels: none\n",
                "jobs: the CI has not fetched zeughaus yet; it does with its first event\n",
                "runners: atik offline\n",
                "platforms: none",
            )
        );

        // No note either: the request did not ask for jobs.
        view.jobs_note.clear();
        view.machines.clear();
        let text = describe(&view, Some(HEAD));
        assert!(text.contains("\njobs: none\nrunners: none\n"), "{text}");
    }

    #[test]
    fn a_job_that_only_runs_in_some_channels_and_one_run_jobs_cannot_run_for_another_reason() {
        let mut view = zeughaus();
        let mut only = job_def("only", &[], &["build-linux-amd64"], "tuco-pc", "host");
        only.channels = vec!["nightly".to_owned(), "release".to_owned()];
        let mut odd = job_def("odd", &["push main"], &[], "tuco-pc", "host");
        odd.manual = false;
        view.jobs = vec![only, odd];
        let text = describe(&view, Some(HEAD));
        assert!(
            text.contains(
                "\n  only  tuco-pc host  needs build-linux-amd64; channels nightly, release\n\
                 \x20 odd   tuco-pc host  on push main; run_jobs cannot run it\n"
            ),
            "{text}"
        );
    }

    /// Everything the extension quotes is ASCII, so a terminal or a model
    /// that cannot render more still reads it.
    #[test]
    fn the_text_is_ascii() {
        assert!(describe(&zeughaus(), Some(HEAD)).is_ascii());
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        use std::process::Command;
        use std::sync::atomic::{AtomicU64, Ordering};

        static COUNTER: AtomicU64 = AtomicU64::new(0);

        /// A directory under the system's temporary one, removed on drop.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(tag: &str) -> Scratch {
                let path = std::env::temp_dir().join(format!(
                    "zeughaus-mcp-repo-{tag}-{}-{}",
                    std::process::id(),
                    COUNTER.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("scratch directory");
                Scratch(path)
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// Git as a test fixture runs it: its own identity, whatever the
        /// developer has configured.
        fn fx(dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=fx", "-c", "user.email=fx@example.invalid"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        fn same_directory(a: &Path, b: &Path) -> bool {
            std::fs::canonicalize(a).expect("a") == std::fs::canonicalize(b).expect("b")
        }

        #[test]
        fn a_checkout_is_named_by_its_origin_without_credentials_and_by_its_head() {
            let base = Scratch::new("named");
            fx(&base.0, &["init", "-q"]);
            fx(
                &base.0,
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://hannes:secret@git.example.invalid/o/demo.git",
                ],
            );
            fx(&base.0, &["commit", "-q", "--allow-empty", "-m", "first"]);
            std::fs::create_dir_all(base.0.join("sub")).expect("subdirectory");

            // From the top and from a directory inside it: the same checkout.
            for path in [base.0.clone(), base.0.join("sub")] {
                let found = checkout(&path).expect("a checkout");
                assert!(same_directory(&found.root, &base.0), "{:?}", found.root);
                assert_eq!(found.origin, "https://git.example.invalid/o/demo.git");
                assert_eq!(found.head, Some(fx(&base.0, &["rev-parse", "HEAD"])));
            }
        }

        #[test]
        fn a_checkout_without_a_commit_has_no_head() {
            let base = Scratch::new("unborn");
            fx(&base.0, &["init", "-q"]);
            fx(
                &base.0,
                &["remote", "add", "origin", "git@example.invalid:o/demo.git"],
            );
            let found = checkout(&base.0).expect("a checkout");
            assert_eq!(found.origin, "git@example.invalid:o/demo.git");
            assert_eq!(found.head, None);
        }

        #[test]
        fn what_is_no_checkout_or_has_no_origin_is_refused_with_the_reason() {
            let base = Scratch::new("refused");
            let err = checkout(&base.0).expect_err("not a repository");
            assert!(err.contains("is not inside a git checkout"), "{err}");
            let err = checkout(&base.0.join("missing")).expect_err("missing");
            assert!(err.contains("is not a directory"), "{err}");

            fx(&base.0, &["init", "-q"]);
            let err = checkout(&base.0).expect_err("no origin");
            assert!(
                err.ends_with("has no origin remote, so no CI repository can be named"),
                "{err}"
            );
        }
    }
}
