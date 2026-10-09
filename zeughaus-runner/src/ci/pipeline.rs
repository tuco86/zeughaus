//! Event patterns, validation of a repository's whole `.zeughaus-ci/` set, and the
//! selection of the jobs an event runs.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use super::CI_DIR;
use super::event::{CiEvent, EventKind};
use super::header::{JobDef, parse_job};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    Push(String),
    Tag(String),
    /// A normalized five-field cron expression.
    Cron(String),
    /// `cron *`: any cron event. Only valid in grants.
    AnyCron,
}

/// Whitespace collapsed to single spaces, so equal schedules compare equal.
pub fn normalize_cron(expr: &str) -> String {
    expr.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `*` matches any sequence including empty and `/`; everything else is
/// literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    // Position after the last `*` and the text index it currently absorbs up to.
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi + 1, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            star = Some((sp, st + 1));
            pi = sp;
            ti = st + 1;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

impl Pattern {
    pub fn parse(text: &str) -> Result<Pattern, String> {
        match Self::parse_grant(text)? {
            Pattern::AnyCron => Err("`cron *` is only valid in grants".into()),
            other => Ok(other),
        }
    }

    pub fn parse_grant(text: &str) -> Result<Pattern, String> {
        let text = text.trim();
        let (kind, rest) = text
            .split_once(char::is_whitespace)
            .ok_or_else(|| format!("pattern `{text}` needs a kind and an argument"))?;
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(format!("pattern `{text}` needs an argument"));
        }
        match kind {
            "push" => Ok(Pattern::Push(single_glob(kind, rest)?)),
            "tag" => Ok(Pattern::Tag(single_glob(kind, rest)?)),
            "cron" => {
                if rest == "*" {
                    return Ok(Pattern::AnyCron);
                }
                let expr = normalize_cron(rest);
                if expr.split(' ').count() != 5 {
                    return Err(format!("cron `{expr}` needs exactly 5 fields"));
                }
                croner::Cron::from_str(&expr).map_err(|e| format!("cron `{expr}`: {e}"))?;
                Ok(Pattern::Cron(expr))
            }
            other => Err(format!(
                "unknown pattern kind `{other}` (push, tag or cron)"
            )),
        }
    }

    pub fn matches(&self, event: &CiEvent) -> bool {
        match self {
            Pattern::Push(glob) => {
                event.kind == EventKind::Push && glob_match(glob, &event.git_ref)
            }
            Pattern::Tag(glob) => event.kind == EventKind::Tag && glob_match(glob, &event.git_ref),
            Pattern::Cron(expr) => {
                event.kind == EventKind::Cron
                    && event.cron.as_deref().map(normalize_cron).as_deref() == Some(expr.as_str())
            }
            Pattern::AnyCron => event.kind == EventKind::Cron,
        }
    }
}

fn single_glob(kind: &str, rest: &str) -> Result<String, String> {
    if rest.contains(char::is_whitespace) {
        return Err(format!("`{kind}` takes one glob, got `{rest}`"));
    }
    Ok(rest.to_string())
}

/// Parses and validates every job of a `.zeughaus-ci/` folder. `files` are the
/// non-Containerfile files as (name, text); `containerfiles` are file names
/// ending in `.Containerfile`. The result is in topological order, ties
/// broken by name.
pub fn load_jobs(
    files: &[(String, String)],
    containerfiles: &[String],
) -> Result<Vec<JobDef>, String> {
    let mut by_name: BTreeMap<String, JobDef> = BTreeMap::new();
    for (file, text) in files {
        let Some(job) = parse_job(file, text)? else {
            continue;
        };
        if let Some(prev) = by_name.get(&job.name) {
            return Err(format!(
                "{CI_DIR}/{file}: job `{}` is already defined by {CI_DIR}/{}",
                job.name, prev.file
            ));
        }
        by_name.insert(job.name.clone(), job);
    }

    for job in by_name.values() {
        validate(job, &by_name, containerfiles)?;
    }
    topological(by_name)
}

fn validate(
    job: &JobDef,
    all: &BTreeMap<String, JobDef>,
    containerfiles: &[String],
) -> Result<(), String> {
    let fail = |msg: String| format!("{CI_DIR}/{}: {msg}", job.file);
    if job.on.is_empty() == job.needs.is_empty() {
        return Err(fail(
            "exactly one of `on` and `needs` must be non-empty".into(),
        ));
    }
    for need in &job.needs {
        if !all.contains_key(need) {
            return Err(fail(format!("needs unknown job `{need}`")));
        }
        if *need == job.name {
            return Err(fail("needs itself".into()));
        }
    }
    if job.image.is_some() && job.machine.is_some() {
        return Err(fail("`image` and `machine` are mutually exclusive".into()));
    }
    let is_ps1 = job.file.ends_with(".ps1");
    if is_ps1 && job.machine.is_none() {
        return Err(fail("a .ps1 job requires `machine`".into()));
    }
    if !is_ps1 && job.machine.is_some() {
        return Err(fail("`machine` requires a .ps1 job".into()));
    }
    if let Some(image) = &job.image {
        let wanted = format!("{image}.Containerfile");
        if !containerfiles.contains(&wanted) {
            return Err(fail(format!("image `{image}` has no {CI_DIR}/{wanted}")));
        }
    }
    if job.when_busy == super::header::WhenBusy::Freeze
        && job.image.is_none()
        && job.machine.is_none()
    {
        return Err(fail(
            "`when_busy = \"freeze\"` requires `image` or `machine`".into(),
        ));
    }
    for pattern in &job.on {
        Pattern::parse(pattern).map_err(|e| fail(format!("on: {e}")))?;
    }
    Ok(())
}

/// Kahn's algorithm; the ready set is ordered by name.
fn topological(by_name: BTreeMap<String, JobDef>) -> Result<Vec<JobDef>, String> {
    let mut waiting: BTreeMap<&str, BTreeSet<&str>> = by_name
        .values()
        .map(|j| {
            (
                j.name.as_str(),
                j.needs.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let mut order: Vec<String> = Vec::with_capacity(by_name.len());
    while !waiting.is_empty() {
        let Some(next) = waiting
            .iter()
            .find(|(_, needs)| needs.is_empty())
            .map(|(name, _)| *name)
        else {
            let files: Vec<String> = waiting
                .keys()
                .filter_map(|name| by_name.get(*name))
                .map(|job| format!("{CI_DIR}/{}", job.file))
                .collect();
            return Err(format!("dependency cycle among jobs: {}", files.join(", ")));
        };
        waiting.remove(next);
        for needs in waiting.values_mut() {
            needs.remove(next);
        }
        order.push(next.to_string());
    }
    let mut by_name = by_name;
    Ok(order
        .into_iter()
        .filter_map(|name| by_name.remove(&name))
        .collect())
}

/// The jobs `event` runs: those with a matching `on`, then every job whose
/// needs are all selected, repeated until nothing changes. `jobs` must be in
/// topological order (as `load_jobs` returns them); so is the result.
pub fn select(jobs: &[JobDef], event: &CiEvent) -> Vec<String> {
    let mut selected: BTreeSet<&str> = jobs
        .iter()
        .filter(|j| {
            j.on.iter()
                .any(|p| Pattern::parse(p).is_ok_and(|p| p.matches(event)))
        })
        .map(|j| j.name.as_str())
        .collect();
    loop {
        let more: Vec<&str> = jobs
            .iter()
            .filter(|j| {
                !selected.contains(j.name.as_str())
                    && !j.needs.is_empty()
                    && j.needs.iter().all(|n| selected.contains(n.as_str()))
            })
            .map(|j| j.name.as_str())
            .collect();
        if more.is_empty() {
            break;
        }
        selected.extend(more);
    }
    jobs.iter()
        .filter(|j| selected.contains(j.name.as_str()))
        .map(|j| j.name.clone())
        .collect()
}

/// Distinct normalized cron expressions of all `on` patterns, sorted.
pub fn cron_schedules(jobs: &[JobDef]) -> Vec<String> {
    let set: BTreeSet<String> = jobs
        .iter()
        .flat_map(|j| j.on.iter())
        .filter_map(|p| match Pattern::parse(p) {
            Ok(Pattern::Cron(expr)) => Some(expr),
            _ => None,
        })
        .collect();
    set.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: EventKind, git_ref: &str, cron: Option<&str>) -> CiEvent {
        CiEvent {
            repo: "r".into(),
            kind,
            git_ref: git_ref.into(),
            sha: None,
            cron: cron.map(Into::into),
            actor: "t".into(),
            delivery: None,
            received: 0,
        }
    }

    fn file(name: &str, header: &str) -> (String, String) {
        let body: String = header.lines().map(|l| format!("# {l}\n")).collect();
        (name.to_string(), format!("# /// ci\n{body}# ///\necho\n"))
    }

    fn load(files: &[(String, String)]) -> Result<Vec<JobDef>, String> {
        load_jobs(files, &["ubuntu.Containerfile".to_string()])
    }

    #[test]
    fn glob() {
        assert!(glob_match("main", "main"));
        assert!(!glob_match("main", "mainx"));
        assert!(glob_match("v*", "v1.2.3"));
        assert!(glob_match("v*", "v"));
        assert!(!glob_match("v*", "x1"));
        assert!(glob_match("*", ""));
        assert!(glob_match("release/*", "release/1/2"));
        assert!(glob_match("a*b*c", "aXbYbZc"));
        assert!(!glob_match("a*b*c", "aXbYbZ"));
        assert!(glob_match("*-fail", "x-fail"));
        assert!(!glob_match("a.c", "abc"));
    }

    #[test]
    fn push_and_tag_patterns() {
        let push = Pattern::parse("push main").unwrap();
        assert!(push.matches(&ev(EventKind::Push, "main", None)));
        assert!(!push.matches(&ev(EventKind::Push, "dev", None)));
        assert!(!push.matches(&ev(EventKind::Tag, "main", None)));
        let tag = Pattern::parse("tag v*").unwrap();
        assert!(tag.matches(&ev(EventKind::Tag, "v1.0", None)));
        assert!(!tag.matches(&ev(EventKind::Push, "v1.0", None)));
    }

    #[test]
    fn cron_patterns_normalize() {
        let p = Pattern::parse("cron  0   4 * * *").unwrap();
        assert_eq!(p, Pattern::Cron("0 4 * * *".into()));
        assert!(p.matches(&ev(EventKind::Cron, "main", Some("0 4  * * *"))));
        assert!(!p.matches(&ev(EventKind::Cron, "main", Some("0 5 * * *"))));
        assert!(!p.matches(&ev(EventKind::Push, "main", None)));
        assert!(Pattern::parse("cron 0 4 * *").is_err());
        assert!(Pattern::parse("cron 99 4 * * *").is_err());
        assert!(Pattern::parse("cron *").is_err());
        assert!(Pattern::parse("nonsense x").is_err());
        assert!(Pattern::parse("push").is_err());
    }

    #[test]
    fn grant_patterns() {
        let any = Pattern::parse_grant("cron *").unwrap();
        assert_eq!(any, Pattern::AnyCron);
        assert!(any.matches(&ev(EventKind::Cron, "main", Some("0 4 * * *"))));
        assert!(!any.matches(&ev(EventKind::Push, "main", None)));
        let tag = Pattern::parse_grant("tag v*").unwrap();
        assert!(tag.matches(&ev(EventKind::Tag, "v2", None)));
    }

    #[test]
    fn loads_in_topological_order() {
        let jobs = load(&[
            file("after.sh", "needs = [\"box\", \"hello\"]"),
            file("hello.sh", "on = [\"push main\"]"),
            file(
                "box.sh",
                "on = [\"push main\"]\nimage = \"ubuntu\"\nwhen_busy = \"freeze\"",
            ),
            ("lib.sh".into(), "foo() { :; }\n".into()),
        ])
        .unwrap();
        let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, ["box", "hello", "after"]);
    }

    #[test]
    fn validation_rules() {
        let on = "on = [\"push main\"]";
        let cases: Vec<(Vec<(String, String)>, &str)> = vec![
            (vec![file("a.sh", "")], "exactly one"),
            (
                vec![
                    file("a.sh", &format!("{on}\nneeds = [\"b\"]")),
                    file("b.sh", on),
                ],
                "exactly one",
            ),
            (vec![file("a.sh", "needs = [\"zzz\"]")], "unknown job"),
            (
                vec![
                    file("a.sh", "needs = [\"b\"]"),
                    file("b.sh", "needs = [\"a\"]"),
                ],
                "cycle",
            ),
            (
                vec![file(
                    "a.sh",
                    &format!("{on}\nimage = \"ubuntu\"\nmachine = \"w\""),
                )],
                "mutually exclusive",
            ),
            (vec![file("a.ps1", on)], "requires `machine`"),
            (
                vec![file("a.sh", &format!("{on}\nmachine = \"w\""))],
                "requires a .ps1",
            ),
            (
                vec![file("a.sh", &format!("{on}\nimage = \"nope\""))],
                "no .zeughaus-ci/nope.Containerfile",
            ),
            (
                vec![file("a.sh", &format!("{on}\nwhen_busy = \"freeze\""))],
                "freeze",
            ),
            (vec![file("a.sh", "on = [\"cron 1 2 3\"]")], "on:"),
            (vec![file("a.sh", "on = [\"cron *\"]")], "on:"),
            (
                vec![
                    file("a.sh", on),
                    file("a.ps1", &format!("{on}\nmachine = \"w\"")),
                ],
                "already defined",
            ),
        ];
        for (files, expect) in cases {
            let err = load(&files).unwrap_err();
            assert!(err.contains(expect), "expected `{expect}` in `{err}`");
            assert!(err.contains(".zeughaus-ci/"), "{err}");
        }
    }

    fn sample() -> Vec<JobDef> {
        load(&[
            file("hello.sh", "on = [\"push main\"]"),
            file("box.sh", "on = [\"push main\", \"tag v*\"]"),
            file("only-tag.sh", "on = [\"tag v*\"]"),
            file("after.sh", "needs = [\"hello\", \"box\"]"),
            file("pub.sh", "needs = [\"after\", \"only-tag\"]"),
            file("nightly.sh", "on = [\"cron 0 4 * * *\"]"),
        ])
        .unwrap()
    }

    #[test]
    fn select_follows_needs() {
        let jobs = sample();
        assert_eq!(
            select(&jobs, &ev(EventKind::Push, "main", None)),
            ["box", "hello", "after"]
        );
        assert_eq!(
            select(&jobs, &ev(EventKind::Tag, "v1", None)),
            ["box", "only-tag"]
        );
    }

    #[test]
    fn job_is_excluded_when_one_need_is_not_selected() {
        let jobs = sample();
        // `after` needs hello, which a tag does not select.
        let tag = select(&jobs, &ev(EventKind::Tag, "v1", None));
        assert!(tag.contains(&"box".to_string()));
        assert!(!tag.contains(&"hello".to_string()));
        assert!(!tag.contains(&"after".to_string()));
        assert!(!tag.contains(&"pub".to_string()));
        assert!(select(&jobs, &ev(EventKind::Push, "dev", None)).is_empty());
    }

    #[test]
    fn select_cron_and_schedules() {
        let jobs = sample();
        assert_eq!(
            select(&jobs, &ev(EventKind::Cron, "main", Some("0  4 * * *"))),
            ["nightly"]
        );
        assert!(select(&jobs, &ev(EventKind::Cron, "main", Some("0 5 * * *"))).is_empty());
        assert_eq!(cron_schedules(&jobs), ["0 4 * * *"]);
    }
}
