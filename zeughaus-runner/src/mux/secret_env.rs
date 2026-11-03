//! The zeughaus secrets a terminal starts with: a work shell gets its
//! project's `shell` document and the global one, an agent session also the
//! `omp` documents, a pane's shell the global `shell` document. They are read
//! with the runner user's own OpenBao credentials through `get-secret`; a
//! runner without them opens its terminals without these variables.
//!
//! Precedence: what the terminal's profile or the runner's own environment
//! already sets, then the documents in the order [`documents`] gives.

use std::collections::BTreeMap;
use std::process::Command;
use std::time::Duration;

use zeughaus_link::ActivityKind;
use zeughaus_link::enlist::secret_line;

/// The KV v2 mount every document is under.
const MOUNT: &str = "secret";

/// How long `get-secret` may take for all documents together.
const FETCH_WAIT: Duration = Duration::from_secs(10);

/// The documents a terminal of `kind` reads, the first one winning; `origin`
/// is the project of a work shell or a session.
pub fn documents(kind: ActivityKind, origin: Option<&str>) -> Vec<String> {
    match (kind, origin) {
        (ActivityKind::Shell, Some(origin)) => {
            vec![format!("zeughaus/{origin}/shell"), "zeughaus/shell".into()]
        }
        (ActivityKind::Shell, None) => vec!["zeughaus/shell".into()],
        (ActivityKind::Session, Some(origin)) => vec![
            format!("zeughaus/{origin}/omp"),
            "zeughaus/omp".into(),
            format!("zeughaus/{origin}/shell"),
            "zeughaus/shell".into(),
        ],
        _ => Vec::new(),
    }
}

/// Adds the fields of `documents` to `env`. `fetched` maps
/// `"<document>/<FIELD>"` to its value. A name `env` already has, an earlier
/// document set, or the runner's own environment has (`present`) is kept as
/// it is; a value over several lines is left out.
pub fn merge(
    env: &mut Vec<(String, String)>,
    documents: &[String],
    fetched: &BTreeMap<String, String>,
    present: impl Fn(&str) -> bool,
) {
    for document in documents {
        let prefix = format!("{document}/");
        for (key, value) in fetched.range(prefix.clone()..) {
            let Some(name) = key.strip_prefix(&prefix) else {
                break;
            };
            // A deeper document below this one is not part of it.
            if name.contains('/') || env.iter().any(|(set, _)| set == name) || present(name) {
                continue;
            }
            if let Some(value) = secret_line(value) {
                env.push((name.to_owned(), value.to_owned()));
            }
        }
    }
}

/// Every field of `documents`, as `"<document>/<FIELD>"` to its value. A
/// document that does not exist has none.
pub fn fetch(documents: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut command = Command::new("get-secret");
    command.arg("--json");
    command.args(documents.iter().map(|doc| format!("{MOUNT}/{doc}")));
    let text = crate::proc::output_within(command, FETCH_WAIT)
        .map_err(|e| format!("get-secret --json: {e}"))?;
    let all: BTreeMap<String, String> = serde_json::from_str(&text)
        .map_err(|e| format!("get-secret --json answered no object of strings: {e}"))?;
    let prefix = format!("{MOUNT}/");
    Ok(all
        .into_iter()
        .filter_map(|(key, value)| Some((key.strip_prefix(&prefix)?.to_owned(), value)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetched(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn documents_follow_the_terminal_kind() {
        assert_eq!(
            documents(ActivityKind::Shell, Some("h/o/r")),
            ["zeughaus/h/o/r/shell", "zeughaus/shell"]
        );
        assert_eq!(documents(ActivityKind::Shell, None), ["zeughaus/shell"]);
        assert_eq!(
            documents(ActivityKind::Session, Some("h/o/r")),
            [
                "zeughaus/h/o/r/omp",
                "zeughaus/omp",
                "zeughaus/h/o/r/shell",
                "zeughaus/shell"
            ]
        );
        for kind in [
            ActivityKind::CiJob,
            ActivityKind::Job,
            ActivityKind::Trigger,
            ActivityKind::Maintenance,
        ] {
            assert!(documents(kind, Some("h/o/r")).is_empty(), "{kind:?}");
        }
    }

    #[test]
    fn the_terminal_env_wins_then_the_project_then_the_global_document() {
        let docs = documents(ActivityKind::Shell, Some("h/o/r"));
        let fetched = fetched(&[
            ("zeughaus/h/o/r/shell/FOO", "project"),
            ("zeughaus/h/o/r/shell/BAR", "project\n"),
            ("zeughaus/shell/FOO", "global"),
            ("zeughaus/shell/BAZ", "global"),
            ("zeughaus/shell/OWN", "global"),
        ]);
        let mut env = vec![("OWN".to_owned(), "mine".to_owned())];
        merge(&mut env, &docs, &fetched, |_| false);
        env.sort();
        assert_eq!(
            env,
            [
                ("BAR".to_owned(), "project".to_owned()),
                ("BAZ".to_owned(), "global".to_owned()),
                ("FOO".to_owned(), "project".to_owned()),
                ("OWN".to_owned(), "mine".to_owned()),
            ]
        );
    }

    #[test]
    fn a_session_takes_omp_before_shell() {
        let docs = documents(ActivityKind::Session, Some("h/o/r"));
        let fetched = fetched(&[
            ("zeughaus/h/o/r/shell/K", "project-shell"),
            ("zeughaus/omp/K", "global-omp"),
            ("zeughaus/shell/L", "global-shell"),
            ("zeughaus/omp/L", "global-omp"),
        ]);
        let mut env = Vec::new();
        merge(&mut env, &docs, &fetched, |_| false);
        env.sort();
        assert_eq!(
            env,
            [
                ("K".to_owned(), "global-omp".to_owned()),
                ("L".to_owned(), "global-omp".to_owned()),
            ]
        );
    }

    #[test]
    fn names_of_the_runners_environment_and_multiline_values_are_skipped() {
        let docs = documents(ActivityKind::Shell, None);
        let fetched = fetched(&[
            ("zeughaus/shell/HOME", "/elsewhere"),
            ("zeughaus/shell/KEY", "a\nb"),
            ("zeughaus/shell/OK", "yes"),
        ]);
        let mut env = Vec::new();
        merge(&mut env, &docs, &fetched, |name| name == "HOME");
        assert_eq!(env, [("OK".to_owned(), "yes".to_owned())]);
    }
}
