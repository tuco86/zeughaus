//! Channels: the named streams a repository sorts its pipelines into
//! (`dev`, `nightly`, `release`, ...), defined in `.zeughaus-ci/channels.toml`.
//!
//! ```toml
//! # the first channel whose `on` matches an event gets its pipeline
//! [[channel]]
//! name = "nightly"
//! on = ["nightly"]
//! # the pipeline the CI's nightly round starts in this channel starts each
//! # place's workspace from nothing; a nightly queued by hand or a rerun
//! # builds on what is there
//! daily_cleanup = true
//! ```
//!
//! An event no channel matches lands in a channel named after what it is
//! (see [`fallback`]), so every pipeline has one.

use std::collections::HashSet;

use serde::Deserialize;

use crate::CI_DIR;
use crate::config::valid_name;
use crate::event::{CiEvent, EventKind};
use crate::pipeline::Pattern;

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
    #[serde(default)]
    daily_cleanup: bool,
    /// Replaced by `daily_cleanup`; parses and does nothing, and `check`
    /// warns about it.
    #[serde(default)]
    clean: Option<bool>,
}

/// A repository's channels in file order.
#[derive(Debug, Clone, Default)]
pub struct Channels(Vec<Entry>);

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    on: Vec<Pattern>,
    daily_cleanup: bool,
    /// The channel still says `clean`.
    ignores_clean: bool,
}

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
                .map(|p| Pattern::parse(p))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| fail(format!("channel `{}`: {e}", def.name)))?;
            if def.daily_cleanup && !patterns.contains(&Pattern::Nightly) {
                return Err(fail(format!(
                    "channel `{}`: `daily_cleanup` needs `nightly` in `on`: only the nightly round cleans up",
                    def.name
                )));
            }
            channels.push(Entry {
                name: def.name,
                on: patterns,
                daily_cleanup: def.daily_cleanup,
                ignores_clean: def.clean.is_some(),
            });
        }
        Ok(Channels(channels))
    }

    /// The first matching channel in file order, else `fallback(event)`.
    pub fn of(&self, event: &CiEvent) -> String {
        self.0
            .iter()
            .find(|entry| entry.on.iter().any(|p| p.matches(event)))
            .map_or_else(|| fallback(event), |entry| entry.name.clone())
    }

    /// Whether `channel` is a channel of the file marked `daily_cleanup`; a
    /// name only [`fallback`] made up is not.
    pub fn daily_cleanup(&self, channel: &str) -> bool {
        self.0
            .iter()
            .any(|entry| entry.daily_cleanup && entry.name == channel)
    }

    /// Name, patterns and `daily_cleanup` flag of every channel in file order.
    pub fn names(&self) -> impl Iterator<Item = (&str, &[Pattern], bool)> {
        self.0.iter().map(|entry| {
            (
                entry.name.as_str(),
                entry.on.as_slice(),
                entry.daily_cleanup,
            )
        })
    }

    /// The channels that still carry `clean`, which does nothing.
    pub fn ignoring_clean(&self) -> impl Iterator<Item = &str> {
        self.0
            .iter()
            .filter(|entry| entry.ignores_clean)
            .map(|entry| entry.name.as_str())
    }
}

/// push `<branch>` -> `<branch>`, tag -> `tags`, nightly -> `nightly`,
/// upstream -> `upstream`, manual run -> its ref, which is the repository's
/// default branch.
pub fn fallback(event: &CiEvent) -> String {
    match event.kind {
        EventKind::Push | EventKind::Manual => event.git_ref.clone(),
        EventKind::Tag => "tags".to_owned(),
        EventKind::Nightly => "nightly".to_owned(),
        EventKind::Upstream => "upstream".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, git_ref: &str) -> CiEvent {
        CiEvent {
            repo: "r".to_owned(),
            kind,
            git_ref: git_ref.to_owned(),
            sha: None,
            actor: "test".to_owned(),
            delivery: None,
            upload: None,
            upstream: None,
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
on = ["nightly"]
[[channel]]
name = "dev"
on = ["push *"]
"#;

    #[test]
    fn first_match_wins_in_file_order() {
        let channels = load(FILE).unwrap();
        assert_eq!(channels.of(&event(EventKind::Tag, "v1")), "release");
        assert_eq!(channels.of(&event(EventKind::Push, "main")), "dev");
        assert_eq!(channels.of(&event(EventKind::Nightly, "main")), "nightly");
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
        assert_eq!(ordered.of(&event(EventKind::Push, "main")), "first");
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
            channels.of(&event(EventKind::Push, "feature/x")),
            "feature/x"
        );
        assert_eq!(channels.of(&event(EventKind::Tag, "nightly-1")), "tags");
        assert_eq!(channels.of(&event(EventKind::Nightly, "main")), "nightly");
        assert_eq!(channels.of(&event(EventKind::Manual, "main")), "main");
        assert_eq!(channels.of(&event(EventKind::Upstream, "main")), "upstream");
    }

    #[test]
    fn no_file_means_fallback_only() {
        let channels = Channels::from_files(&[("build.sh".to_owned(), String::new())]).unwrap();
        assert_eq!(channels.names().count(), 0);
        assert_eq!(channels.of(&event(EventKind::Push, "main")), "main");
        assert_eq!(channels.of(&event(EventKind::Tag, "v1")), "tags");
    }

    #[test]
    fn daily_cleanup_marks_a_channel_and_no_fallback() {
        let channels = load(
            r#"
[[channel]]
name = "nightly"
on = ["nightly"]
daily_cleanup = true
[[channel]]
name = "dev"
on = ["push main"]
"#,
        )
        .unwrap();
        assert!(channels.daily_cleanup("nightly"));
        assert!(!channels.daily_cleanup("dev"));
        // Named after a branch no channel matched: nothing marked it.
        assert!(!channels.daily_cleanup("feature/x"));
        assert!(!Channels::default().daily_cleanup("nightly"));
        let listed: Vec<(&str, bool)> = channels
            .names()
            .map(|(name, _, daily)| (name, daily))
            .collect();
        assert_eq!(listed, [("nightly", true), ("dev", false)]);
    }

    #[test]
    fn the_old_clean_key_still_parses_and_marks_nothing() {
        let channels = load(
            r#"
[[channel]]
name = "release"
on = ["tag v*"]
clean = true
[[channel]]
name = "dev"
on = ["push main"]
"#,
        )
        .unwrap();
        assert!(!channels.daily_cleanup("release"));
        assert_eq!(channels.ignoring_clean().collect::<Vec<_>>(), ["release"]);
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
            (
                "daily_cleanup type",
                "[[channel]]\nname = \"a\"\non = [\"nightly\"]\ndaily_cleanup = \"yes\"\n",
            ),
            // The tag of a release never is the nightly round.
            (
                "daily_cleanup without nightly",
                "[[channel]]\nname = \"a\"\non = [\"tag v*\"]\ndaily_cleanup = true\n",
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
