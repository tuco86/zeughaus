//! `zeughaus-ci [--state-dir DIR] <command>`: running the service, checking
//! a `.zeughaus-ci` folder, queueing a pipeline, reading their state, and
//! the forge token and OpenBao checks.
//!
//! ```text
//! serve --listen <host:port> [--hook <host:port>]
//! check [DIR]
//! plan [DIR] <push|tag> <ref>
//! plan [DIR] cron "<expr>"
//! run <repo> <push|tag> <ref> [<sha>]
//! run <repo> cron "<expr>"
//! status [N]
//! log <repo> <pipeline> <job> [--tail N]
//! forge-check <repo>
//! secrets-check
//! ```
//!
//! `check` reads the folder, and the state directory's `ci.toml` when there
//! is one, to place every job as a pipeline would; `plan` reads only the
//! folder; the others read `ci.toml`. Only `serve` is the service: `run`
//! drops an event into the inbox its scheduler drains, and `status` and
//! `log` read its records.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use zeughaus_link::credentials;

use crate::CI_DIR;
use crate::channels::{CHANNELS_FILE, Channels};
use crate::config::CiConfig;
use crate::event::{self, CiEvent, EventKind};
use crate::excerpt;
use crate::header::{self, JobDef};
use crate::pipeline::{self, Pattern};
use crate::scheduler::{self, JobStatus, PipelineStatus, place_job, place_name};
use crate::secrets::{self, Secrets};
use crate::streak;

const USAGE: &str = "usage: zeughaus-ci [--state-dir DIR] serve --listen <host:port> \
[--hook <host:port>] | check [DIR] | plan [DIR] <push|tag> <ref> | plan [DIR] cron \"<expr>\" | \
run <repo> <push|tag> <ref> [<sha>] | run <repo> cron \"<expr>\" | status [N] | \
log <repo> <pipeline> <job> [--tail N] | forge-check <repo> | secrets-check";

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (state_dir, args) = match split_state_dir(&args) {
        Ok(split) => split,
        Err(e) => {
            eprintln!("[ci] {e}");
            return ExitCode::FAILURE;
        }
    };
    run(args, &state_dir)
}

/// The leading `--state-dir <path>`, else the state directory every zeughaus
/// process shares (`ZEUGHAUS_STATE_DIR` or the platform's default), and the
/// arguments after it.
fn split_state_dir(args: &[String]) -> Result<(PathBuf, &[String]), String> {
    match args {
        [flag, rest @ ..] if flag == "--state-dir" => match rest {
            [dir, rest @ ..] => Ok((PathBuf::from(dir), rest)),
            [] => Err("--state-dir needs a path argument".to_owned()),
        },
        _ => Ok((credentials::state_dir(), args)),
    }
}

fn run(args: &[String], state_dir: &Path) -> ExitCode {
    let result = match args.first().map(String::as_str) {
        Some("serve") => return crate::service::run(&args[1..], state_dir),
        Some("check") => check(state_dir, &args[1..]),
        Some("plan") => plan(&args[1..]),
        Some("run") => queue(state_dir, &args[1..]),
        Some("status") => status(state_dir, &args[1..]),
        Some("log") => log(state_dir, &args[1..]),
        Some("forge-check") => forge_check(state_dir, &args[1..]),
        Some("secrets-check") => secrets_check(state_dir, &args[1..]),
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

/// The jobs and channels of a `.zeughaus-ci` folder on disk, validated as a
/// pipeline would be.
fn load_dir(dir: &Path) -> Result<(Vec<JobDef>, Channels), String> {
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
    let jobs = pipeline::load_jobs(&files, &containerfiles)?;
    Ok((jobs, Channels::from_files(&files)?))
}

/// What `check` says about the header keys that parse and do nothing, one
/// line per key.
fn ignored_key_warnings(jobs: &[JobDef]) -> Vec<String> {
    jobs.iter()
        .flat_map(|job| {
            job.ignored_keys.iter().map(move |key| {
                format!(
                    "{CI_DIR}/{}: `{key}` is ignored: a repository's jobs at one place share one workspace, and a checkout keeps ignored files",
                    job.file
                )
            })
        })
        .collect()
}

/// Every channel a job names exists: in `channels.toml`, or one of the
/// fallback names an event without a listed channel gets.
fn check_job_channels(jobs: &[JobDef], channels: &Channels) -> Result<(), String> {
    let known =
        |c: &str| matches!(c, "tags" | "cron") || channels.names().any(|(name, ..)| name == c);
    for job in jobs {
        if let Some(c) = job.channels.iter().find(|c| !known(c)) {
            return Err(format!(
                "{CI_DIR}/{}: channel `{c}` is not in {CHANNELS_FILE} and is no fallback name (tags, cron)",
                job.file
            ));
        }
    }
    Ok(())
}

/// Validates the folder as a pipeline would, placement included when the
/// state directory has a `ci.toml`: whether `freeze` fits a job's `platform`
/// or `machine` depends on what `ci.toml` maps it to.
fn check(state_dir: &Path, args: &[String]) -> Result<(), String> {
    let dir = match args {
        [] => CI_DIR,
        [dir] => dir.as_str(),
        _ => return Err("usage: zeughaus-ci check [DIR]".to_owned()),
    };
    let dir = Path::new(dir);
    let (jobs, channels) = load_dir(dir)?;
    for warning in ignored_key_warnings(&jobs) {
        eprintln!("[ci] warning: {warning}");
    }
    check_job_channels(&jobs, &channels)?;
    let config = match CiConfig::load(state_dir) {
        Ok(Some(config)) => Some(config),
        Ok(None) => {
            eprintln!(
                "[ci] no {} in {}: placement not checked",
                crate::config::CONFIG_FILE,
                state_dir.display()
            );
            None
        }
        Err(e) => {
            eprintln!("[ci] placement not checked: {e}");
            None
        }
    };
    match &config {
        Some(config) => {
            let problems: Vec<String> = jobs
                .iter()
                .filter_map(|job| {
                    place_job(config, "check", job, |image| {
                        let path = dir.join(format!("{image}.Containerfile"));
                        std::fs::read_to_string(&path)
                            .map_err(|e| format!("cannot read {}: {e}", path.display()))
                    })
                    .err()
                })
                .collect();
            if !problems.is_empty() {
                return Err(problems.join("\n"));
            }
        }
        None => {
            for job in jobs.iter().filter(|job| {
                job.when_busy == zeughaus_link::enlist::WhenBusy::Freeze && job.image.is_none()
            }) {
                eprintln!(
                    "[ci] warning: {CI_DIR}/{}: when_busy = \"freeze\" needs a container or a VM; \
                     {} must name a VM in ci.toml",
                    job.file,
                    place_name(job)
                );
            }
        }
    }
    for job in jobs {
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
            header::when_busy_name(job.when_busy)
        );
    }
    for (name, patterns, clean) in channels.names() {
        let on: Vec<String> = patterns.iter().map(Pattern::to_string).collect();
        let clean = if clean { " (clean)" } else { "" };
        println!("channel {name}{clean}  on: {}", on.join(", "));
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
        ("manual", _) => return Err("manual events come from the /manual service".to_owned()),
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
        upload: None,
        received: crate::now_secs(),
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
    let (jobs, channels) = load_dir(Path::new(dir))?;
    println!("channel {}", channels.of(&event));
    let selected = pipeline::select(&jobs, &event, &channels.of(&event));
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
            crate::config::CONFIG_FILE,
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
/// pipeline of each repository shows the excerpt of
/// every failed job; older ones keep to one line per job.
fn status(state_dir: &Path, args: &[String]) -> Result<(), String> {
    let count = match args {
        [] => 10,
        [n] => n.parse().map_err(|_| format!("{n:?} is not a number"))?,
        _ => return Err("usage: zeughaus-ci status [N]".to_owned()),
    };
    let now = crate::now_secs();
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
            "{} #{} {} {} {} {} {}",
            pipeline.repo,
            pipeline.number,
            pipeline.channel(),
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
                "  {:<20} {:<10} {:<14} {}",
                job.def.name,
                job.status.as_str(),
                job.runner.as_deref().unwrap_or("-"),
                job.note
            );
            if !newest || job.status != JobStatus::Failed {
                continue;
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
    const USAGE: &str = "usage: zeughaus-ci log <repo> <pipeline> <job> [--tail N]";
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
    if !pipeline.jobs.iter().any(|j| j.def.name == *job) {
        return Err(format!("{repo} #{number} has no job {job}"));
    }
    let dir = scheduler::transcript_dir(state_dir, repo, number, job);
    if !dir.join("wal").exists() {
        return Err(format!("{repo} #{number} {job} has no transcript"));
    }
    let text = excerpt::read_log(&dir)?;
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
        return Err("usage: zeughaus-ci forge-check <repo>".to_owned());
    };
    let config = load_config(state_dir)?;
    let repo_config = config
        .repos
        .get(repo)
        .ok_or_else(|| format!("unknown repo {repo}"))?;
    let secrets = Secrets::new(config.secrets.clone(), state_dir);
    println!("{}", crate::forge::check(&secrets, repo_config)?);
    Ok(())
}

/// Every secret `ci.toml` names, read from OpenBao once.
fn secrets_check(state_dir: &Path, args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err("usage: zeughaus-ci secrets-check".to_owned());
    }
    let config = load_config(state_dir)?;
    let secrets = Secrets::new(config.secrets.clone(), state_dir);
    for line in secrets::check(&secrets, &config)? {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job_in(channels: &str) -> JobDef {
        let text = format!("# /// ci\n# needs = [\"a\"]\n# channels = [{channels}]\n# ///\n");
        header::parse_job("b.sh", &text).unwrap().unwrap()
    }

    #[test]
    fn a_job_may_name_listed_channels_and_the_fallback_names_only() {
        let channels = Channels::from_files(&[(
            CHANNELS_FILE.to_owned(),
            "[[channel]]\nname = \"nightly\"\non = [\"cron *\"]\n".to_owned(),
        )])
        .unwrap();
        let ok = job_in("\"nightly\", \"tags\", \"cron\"");
        assert!(check_job_channels(&[ok], &channels).is_ok());
        let err = check_job_channels(&[job_in("\"nightlyy\"")], &channels).unwrap_err();
        assert!(
            err.contains("channel `nightlyy` is not in channels.toml"),
            "{err}"
        );
        assert!(check_job_channels(&[job_in("\"manual\"")], &channels).is_err());
    }

    #[test]
    fn the_keys_a_header_carries_for_nothing_are_warned_about() {
        let text =
            "# /// ci\n# on = [\"push main\"]\n# context = \"x\"\n# cache = [\".c\"]\n# ///\n";
        let job = header::parse_job("a.sh", text).unwrap().unwrap();
        let plain = header::parse_job("b.sh", "# /// ci\n# on = [\"push main\"]\n# ///\n")
            .unwrap()
            .unwrap();
        let warnings = ignored_key_warnings(&[job, plain]);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].starts_with(".zeughaus-ci/a.sh: `context` is ignored: "));
        assert!(warnings[1].starts_with(".zeughaus-ci/a.sh: `cache` is ignored: "));
    }

    #[test]
    fn manual_events_cannot_be_queued_by_hand() {
        let err = parse_event("r", "manual", &[], "main").unwrap_err();
        assert_eq!(err, "manual events come from the /manual service");
    }
}
