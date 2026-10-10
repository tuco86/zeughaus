//! Channels: the named streams a repository sorts its pipelines into
//! (`dev`, `nightly`, `release`, ...), defined in `.zeughaus-ci/channels.toml`.
//!
//! ```toml
//! # the first channel whose `on` matches an event gets its pipeline
//! [[channel]]
//! name = "release"
//! on = ["tag v*"]
//! ```
//!
//! An event no channel matches lands in a channel named after what it is
//! (see [`fallback`]), so every pipeline has one.

use std::collections::HashSet;

use serde::Deserialize;

use super::CI_DIR;
use super::config::valid_name;
use super::event::{CiEvent, EventKind};
use super::pipeline::Pattern;

pub const CHANNELS_FILE: &str = "channels.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    channel: Vec<Def>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Def {
    name: String,
    on: Vec<String>,
}

/// A repository's channels in file order.
#[derive(Debug, Clone, Default)]
pub struct Channels(Vec<(String, Vec<Pattern>)>);

impl Channels {
    /// From the files `read_ci` returned; no channels.toml is no channels.
    pub fn from_files(files: &[(String, String)]) -> Result<Channels, String> {
        let Some((_, text)) = files.iter().find(|(name, _)| name == CHANNELS_FILE) else {
            return Ok(Channels::default());
        };
        let fail = |what: String| format!("{CI_DIR}/{CHANNELS_FILE}: {what}");
        let file: File = toml::from_str(text).map_err(|e| fail(e.to_string()))?;
        let mut seen = HashSet::new();
        let mut channels = Vec::new();
        for def in file.channel {
            if !valid_name(&def.name) {
                return Err(fail(format!(
                    "channel name `{}` must match [a-z0-9][a-z0-9-]*",
                    def.name
                )));
            }
            if !seen.insert(def.name.clone()) {
                return Err(fail(format!("channel `{}` is defined twice", def.name)));
            }
            if def.on.is_empty() {
                return Err(fail(format!("channel `{}` has an empty `on`", def.name)));
            }
            let patterns = def
                .on
                .iter()
                .map(|p| Pattern::parse_grant(p))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| fail(format!("channel `{}`: {e}", def.name)))?;
            channels.push((def.name, patterns));
        }
        Ok(Channels(channels))
    }

    /// The first matching channel in file order, else `fallback(event)`.
    pub fn of(&self, event: &CiEvent) -> String {
        self.0
            .iter()
            .find(|(_, patterns)| patterns.iter().any(|p| p.matches(event)))
            .map_or_else(|| fallback(event), |(name, _)| name.clone())
    }

    pub fn names(&self) -> impl Iterator<Item = (&str, &[Pattern])> {
        self.0.iter().map(|(n, p)| (n.as_str(), p.as_slice()))
    }
}

/// push `<branch>` -> `<branch>`, tag -> `tags`, cron -> `cron`.
pub fn fallback(event: &CiEvent) -> String {
    match event.kind {
        EventKind::Push => event.git_ref.clone(),
        EventKind::Tag => "tags".to_owned(),
        EventKind::Cron => "cron".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, git_ref: &str, cron: Option<&str>) -> CiEvent {
        CiEvent {
            repo: "r".to_owned(),
            kind,
            git_ref: git_ref.to_owned(),
            sha: None,
            cron: cron.map(str::to_owned),
            actor: "test".to_owned(),
            delivery: None,
            received: 0,
        }
    }

    fn load(text: &str) -> Result<Channels, String> {
        Channels::from_files(&[(CHANNELS_FILE.to_owned(), text.to_owned())])
    }

    const FILE: &str = r#"
[[channel]]
name = "release"
on = ["tag v*"]
[[channel]]
name = "nightly"
on = ["cron *"]
[[channel]]
name = "dev"
on = ["push *"]
"#;

    #[test]
    fn first_match_wins_in_file_order() {
        let channels = load(FILE).unwrap();
        assert_eq!(channels.of(&event(EventKind::Tag, "v1", None)), "release");
        assert_eq!(channels.of(&event(EventKind::Push, "main", None)), "dev");
        assert_eq!(
            channels.of(&event(EventKind::Cron, "main", Some("0 3 * * *"))),
            "nightly"
        );
        // Two channels whose patterns both match: file order decides.
        let ordered = load(
            r#"
[[channel]]
name = "first"
on = ["push *"]
[[channel]]
name = "second"
on = ["push main"]
"#,
        )
        .unwrap();
        assert_eq!(ordered.of(&event(EventKind::Push, "main", None)), "first");
    }

    #[test]
    fn unmatched_events_fall_back() {
        let channels = load(
            r#"
[[channel]]
name = "release"
on = ["tag v*"]
"#,
        )
        .unwrap();
        assert_eq!(
            channels.of(&event(EventKind::Push, "feature/x", None)),
            "feature/x"
        );
        assert_eq!(
            channels.of(&event(EventKind::Tag, "nightly-1", None)),
            "tags"
        );
        assert_eq!(
            channels.of(&event(EventKind::Cron, "main", Some("0 3 * * *"))),
            "cron"
        );
    }

    #[test]
    fn no_file_means_fallback_only() {
        let channels = Channels::from_files(&[("build.sh".to_owned(), String::new())]).unwrap();
        assert_eq!(channels.names().count(), 0);
        assert_eq!(channels.of(&event(EventKind::Push, "main", None)), "main");
        assert_eq!(channels.of(&event(EventKind::Tag, "v1", None)), "tags");
    }

    #[test]
    fn invalid_files_are_rejected_with_the_prefix() {
        let cases = [
            (
                "duplicate",
                "[[channel]]\nname = \"a\"\non = [\"push *\"]\n[[channel]]\nname = \"a\"\non = [\"tag *\"]\n",
            ),
            ("name", "[[channel]]\nname = \"Dev\"\non = [\"push *\"]\n"),
            ("empty on", "[[channel]]\nname = \"a\"\non = []\n"),
            (
                "pattern",
                "[[channel]]\nname = \"a\"\non = [\"branch main\"]\n",
            ),
        ];
        for (what, text) in cases {
            let err = load(text).unwrap_err();
            assert!(
                err.starts_with(".zeughaus-ci/channels.toml: "),
                "{what}: {err}"
            );
        }
    }
}
