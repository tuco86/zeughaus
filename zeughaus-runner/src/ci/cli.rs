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
//! ci forge-check <repo>
//! ci hook --listen <addr>
//! ```
//!
//! `check` and `plan` read only the folder; the others read the state
//! directory's `ci.toml`. None of them talks to a running runner: `run`
//! drops an event into the inbox the scheduler drains.

use std::path::Path;
use std::process::ExitCode;

use super::CI_DIR;
use super::config::CiConfig;
use super::event::{self, CiEvent, EventKind};
use super::header::JobDef;
use super::pipeline::{self, Pattern};
use super::scheduler::{self, place_name};

const USAGE: &str = "usage: zeughaus-runner ci check [DIR] | plan [DIR] <push|tag> <ref> | \
plan [DIR] cron \"<expr>\" | run <repo> <push|tag> <ref> [<sha>] | run <repo> cron \"<expr>\" | \
status [N] | forge-check <repo> | hook --listen <addr>";

pub fn run(args: &[String], state_dir: &Path) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("check") => check(&args[1..]),
        Some("plan") => plan(&args[1..]),
        Some("run") => queue(state_dir, &args[1..]),
        Some("status") => status(state_dir, &args[1..]),
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

fn status(state_dir: &Path, args: &[String]) -> Result<(), String> {
    let count = match args {
        [] => 10,
        [n] => n.parse().map_err(|_| format!("{n:?} is not a number"))?,
        _ => return Err("usage: ci status [N]".to_owned()),
    };
    for pipeline in scheduler::load_pipelines(state_dir).into_iter().take(count) {
        let sha = pipeline.sha.as_deref().unwrap_or("-");
        println!(
            "{} #{} {} {} {} {}",
            pipeline.repo,
            pipeline.number,
            pipeline.event.kind,
            pipeline.event.git_ref,
            &sha[..sha.len().min(7)],
            pipeline.status.as_str()
        );
        if !pipeline.note.is_empty() {
            println!("  {}", pipeline.note);
        }
        for job in &pipeline.jobs {
            println!(
                "  {:<20} {:<10} {}",
                job.def.name,
                job.status.as_str(),
                job.note
            );
        }
    }
    Ok(())
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
