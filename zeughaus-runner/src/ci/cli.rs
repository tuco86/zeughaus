//! `zeughaus-runner ci ...`: checking a `.zeughaus-ci` folder, queueing a pipeline,
//! reading their state, the forge token check and the webhook intake.
//!
//! ```text
//! ci check [DIR]
//! ci plan [DIR] <push|tag> <ref>
//! ci plan [DIR] cron "<expr>"
//! ci run <repo> <push|tag> <ref> [<sha>]
//! ci run <repo> cron "<expr>"
//! ci status [N]
//! ci log <repo> <pipeline> <job> [--tail N]
//! ci forge-check <repo>
//! ci hook --listen <addr>
//! ```
//!
//! `check` and `plan` read only the folder; the others read the state
//! directory's `ci.toml`. None of them talks to a running runner: `run`
//! drops an event into the inbox the scheduler drains.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::ExitCode;

use super::CI_DIR;
use super::config::CiConfig;
use super::event::{self, CiEvent, EventKind};
use super::excerpt;
use super::header::JobDef;
use super::pipeline::{self, Pattern};
use super::scheduler::{self, JobStatus, PipelineStatus, place_name};
use super::streak;

const USAGE: &str = "usage: zeughaus-runner ci check [DIR] | plan [DIR] <push|tag> <ref> | \
plan [DIR] cron \"<expr>\" | run <repo> <push|tag> <ref> [<sha>] | run <repo> cron \"<expr>\" | \
status [N] | log <repo> <pipeline> <job> [--tail N] | forge-check <repo> | hook --listen <addr>";

pub fn run(args: &[String], state_dir: &Path) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("check") => check(&args[1..]),
        Some("plan") => plan(&args[1..]),
        Some("run") => queue(state_dir, &args[1..]),
        Some("status") => status(state_dir, &args[1..]),
        Some("log") => log(state_dir, &args[1..]),
        Some("forge-check") => forge_check(state_dir, &args[1..]),
        Some("hook") => return super::hook::run(state_dir, &args[1..]),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[ci] {e}");
            ExitCode::FAILURE
        }
    }
}

/// The jobs of a `.zeughaus-ci` folder on disk, validated as a pipeline would be.
fn load_dir(dir: &Path) -> Result<Vec<JobDef>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut files = Vec::new();
    let mut containerfiles = Vec::new();
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".Containerfile") {
            containerfiles.push(name);
            continue;
        }
        let bytes = std::fs::read(entry.path())
            .map_err(|e| format!("cannot read {}: {e}", entry.path().display()))?;
        files.push((name, String::from_utf8_lossy(&bytes).into_owned()));
    }
    files.sort();
    containerfiles.sort();
    pipeline::load_jobs(&files, &containerfiles)
}

fn check(args: &[String]) -> Result<(), String> {
    let dir = match args {
        [] => CI_DIR,
        [dir] => dir.as_str(),
        _ => return Err("usage: ci check [DIR]".to_owned()),
    };
    for job in load_dir(Path::new(dir))? {
        let triggers = if job.on.is_empty() {
            format!("needs: {}", job.needs.join(", "))
        } else {
            format!("on: {}", job.on.join(", "))
        };
        println!(
            "{:<20} {:<20} {:<40} {}",
            job.name,
            place_name(&job),
            triggers,
            job.when_busy
        );
    }
    Ok(())
}

/// Parses `<push|tag> <ref>` or `cron "<expr>"` into an event of `repo`.
fn parse_event(
    repo: &str,
    kind: &str,
    rest: &[String],
    default_branch: &str,
) -> Result<CiEvent, String> {
    let (kind, git_ref, sha, cron) = match (kind, rest) {
        ("push", [git_ref]) => (EventKind::Push, git_ref.clone(), None, None),
        ("push", [git_ref, sha]) => (EventKind::Push, git_ref.clone(), Some(sha.clone()), None),
        ("tag", [git_ref]) => (EventKind::Tag, git_ref.clone(), None, None),
        ("tag", [git_ref, sha]) => (EventKind::Tag, git_ref.clone(), Some(sha.clone()), None),
        ("cron", [expr]) => {
            let pattern = Pattern::parse(&format!("cron {expr}"))?;
            let Pattern::Cron(expr) = pattern else {
                return Err(format!("{expr:?} is not a cron expression"));
            };
            (EventKind::Cron, default_branch.to_owned(), None, Some(expr))
        }
        _ => return Err(USAGE.to_owned()),
    };
    Ok(CiEvent {
        repo: repo.to_owned(),
        kind,
        git_ref,
        sha,
        cron,
        actor: "manual".to_owned(),
        delivery: None,
        received: super::now_secs(),
    })
}

fn plan(args: &[String]) -> Result<(), String> {
    let starts_event = |arg: &String| matches!(arg.as_str(), "push" | "tag" | "cron");
    let (dir, rest) = match args.first() {
        Some(first) if !starts_event(first) => (first.as_str(), &args[1..]),
        _ => (CI_DIR, args),
    };
    let [kind, rest @ ..] = rest else {
        return Err(USAGE.to_owned());
    };
    if rest.len() != 1 {
        return Err(USAGE.to_owned());
    }
    let event = parse_event("", kind, rest, "main")?;
    let jobs = load_dir(Path::new(dir))?;
    let selected = pipeline::select(&jobs, &event);
    if selected.is_empty() {
        println!("no jobs");
    } else {
        println!("{}", selected.join(" "));
    }
    Ok(())
}

fn load_config(state_dir: &Path) -> Result<CiConfig, String> {
    CiConfig::load(state_dir)?.ok_or_else(|| {
        format!(
            "no {} in {}",
            super::config::CONFIG_FILE,
            state_dir.display()
        )
    })
}

fn queue(state_dir: &Path, args: &[String]) -> Result<(), String> {
    let [repo, kind, rest @ ..] = args else {
        return Err(USAGE.to_owned());
    };
    let config = load_config(state_dir)?;
    let repo_config = config
        .repos
        .get(repo)
        .ok_or_else(|| format!("unknown repo {repo}"))?;
    let event = parse_event(repo, kind, rest, &repo_config.default_branch)?;
    event::write_inbox(state_dir, &event)?;
    println!("queued");
    Ok(())
}

/// The default branch of every configured repository first (green, or which
/// jobs are red since when), then the newest `N` pipelines. The newest
/// pipeline of each repository shows the run directory and the excerpt of
/// every failed job; older ones keep to one line per job.
fn status(state_dir: &Path, args: &[String]) -> Result<(), String> {
    let count = match args {
        [] => 10,
        [n] => n.parse().map_err(|_| format!("{n:?} is not a number"))?,
        _ => return Err("usage: ci status [N]".to_owned()),
    };
    let now = super::now_secs();
    let mut out = String::new();
    if let Some(config) = CiConfig::load(state_dir)? {
        for (name, repo) in &config.repos {
            let pipelines = scheduler::load_repo_pipelines(state_dir, name);
            if pipelines.is_empty() {
                continue;
            }
            let branch = &repo.default_branch;
            let mut line = streak::of_branch(&pipelines, branch).describe(now);
            // The streak counts pipelines with jobs; a newest push without
            // a CI folder means none of it is running any more.
            if let Some(newest) = pipelines
                .iter()
                .filter(|p| streak::is_branch(p, branch))
                .max_by_key(|p| p.number)
                && newest.status == PipelineStatus::NoJobs
            {
                let _ = write!(line, "; #{}: {}", newest.number, newest.note);
            }
            let _ = writeln!(out, "{name} {branch}: {line}");
        }
        out.push('\n');
    }
    let mut seen = HashSet::new();
    for pipeline in scheduler::load_pipelines(state_dir).into_iter().take(count) {
        // A record without jobs (a setup error, no CI folder) has nothing
        // to explain; the newest one that ran something gets the long form.
        let newest = !pipeline.jobs.is_empty() && seen.insert(pipeline.repo.clone());
        let sha = pipeline.sha.as_deref().unwrap_or("-");
        let _ = writeln!(
            out,
            "{} #{} {} {} {} {}",
            pipeline.repo,
            pipeline.number,
            pipeline.event.kind,
            pipeline.event.git_ref,
            &sha[..sha.len().min(7)],
            pipeline.status.as_str()
        );
        if !pipeline.note.is_empty() {
            let _ = writeln!(out, "  {}", pipeline.note);
        }
        for job in &pipeline.jobs {
            let _ = writeln!(
                out,
                "  {:<20} {:<10} {}",
                job.def.name,
                job.status.as_str(),
                job.note
            );
            if !newest || job.status != JobStatus::Failed {
                continue;
            }
            if let Some(run_dir) = &job.run_dir {
                let _ = writeln!(out, "    run {}", run_dir.display());
            }
            for line in job.excerpt.iter().flat_map(|e| e.lines()) {
                let _ = writeln!(out, "    | {line}");
            }
        }
    }
    print_out(&out);
    Ok(())
}

/// A job's log as plain text, whole or its last `N` lines.
fn log(state_dir: &Path, args: &[String]) -> Result<(), String> {
    const USAGE: &str = "usage: ci log <repo> <pipeline> <job> [--tail N]";
    let (repo, number, job, tail) = match args {
        [repo, number, job] => (repo, number, job, None),
        [repo, number, job, flag, n] if flag == "--tail" => (
            repo,
            number,
            job,
            Some(
                n.parse::<usize>()
                    .map_err(|_| format!("{n:?} is not a number"))?,
            ),
        ),
        _ => return Err(USAGE.to_owned()),
    };
    let number = number
        .trim_start_matches('#')
        .parse::<u64>()
        .map_err(|_| format!("{number:?} is not a pipeline number"))?;
    let pipeline = scheduler::load_pipeline(state_dir, repo, number)?;
    let record = pipeline
        .jobs
        .iter()
        .find(|j| j.def.name == *job)
        .ok_or_else(|| format!("{repo} #{number} has no job {job}"))?;
    let run_dir = record
        .run_dir
        .as_ref()
        .ok_or_else(|| format!("{repo} #{number} {job} never ran"))?;
    let text = excerpt::read_log(&run_dir.join("log"))?;
    let lines: Vec<&str> = text.lines().collect();
    let start = tail.map_or(0, |n| lines.len().saturating_sub(n));
    let mut out = lines[start..].join("\n");
    out.push('\n');
    print_out(&out);
    Ok(())
}

/// Writes to stdout, quietly stopping when the reader went away (`| head`).
fn print_out(text: &str) {
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
}

fn forge_check(state_dir: &Path, args: &[String]) -> Result<(), String> {
    let [repo] = args else {
        return Err("usage: ci forge-check <repo>".to_owned());
    };
    let config = load_config(state_dir)?;
    let repo_config = config
        .repos
        .get(repo)
        .ok_or_else(|| format!("unknown repo {repo}"))?;
    println!("{}", super::forge::check(state_dir, repo_config)?);
    Ok(())
}
