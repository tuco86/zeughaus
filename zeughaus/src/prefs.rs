//! What this editor looks like, kept per machine.
//!
//! Native only: a browser tab has no state directory, so the bundled pack is
//! the whole choice there and nothing outlives the tab.
//!
//! Two things live under the state directory: `editor.toml`, one flat table
//! small enough to edit by hand, and `themes/`, whose `*.toml` files are
//! WezTerm colour schemes -- the format every iTerm2-Color-Schemes entry is
//! published in, so a scheme is dropped in rather than converted.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use iced_tabs::Placement;
use serde::{Deserialize, Serialize};
use zeughaus_theme::Theme;

/// The preferences file under the state directory.
const FILE: &str = "editor.toml";
/// The directory user themes are read from.
const THEMES: &str = "themes";
/// The file listing the remote runners this editor connects to besides the
/// one of this machine.
const CONFIG: &str = "zeughaus.toml";

/// What one editor window starts as.
///
/// `serde(default)`: the file is edited by hand, and a table naming only the
/// theme must keep the placement rather than lose both to one missing key.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The theme's name, matched against the pack and then the file themes. A
    /// name nothing answers to leaves the default in place: a theme removed
    /// from `themes/` must not stop the editor from opening.
    pub theme: String,
    pub tabs: Tabs,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: Theme::default().name().to_owned(),
            tabs: Tabs::Top,
        }
    }
}

/// Where the tab bar sits. Its own type because [`Placement`] belongs to
/// `iced_tabs` and cannot carry a serde impl of this crate's.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tabs {
    Top,
    Left,
}

impl From<Placement> for Tabs {
    fn from(placement: Placement) -> Self {
        match placement {
            Placement::Top => Tabs::Top,
            Placement::Left => Tabs::Left,
        }
    }
}

impl From<Tabs> for Placement {
    fn from(tabs: Tabs) -> Self {
        match tabs {
            Tabs::Top => Placement::Top,
            Tabs::Left => Placement::Left,
        }
    }
}

/// Where `editor.toml` lives.
fn file() -> PathBuf {
    zeughaus_link::credentials::state_dir().join(FILE)
}

/// What the last session left, or the defaults.
///
/// A file that cannot be read or parsed is reported once and then ignored: a
/// preference is not worth refusing to start over.
pub fn load() -> Prefs {
    let path = file();
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Prefs::default(),
        Err(e) => {
            eprintln!("[prefs] {}: {e} -- using the defaults", path.display());
            return Prefs::default();
        }
    };
    match toml::from_str(&text) {
        Ok(prefs) => prefs,
        Err(e) => {
            eprintln!("[prefs] {}: {e} -- using the defaults", path.display());
            Prefs::default()
        }
    }
}

/// Records the theme and the tab placement. Best effort: a preference that
/// cannot be written earns one line on stderr and nothing else.
pub fn save(theme: &str, placement: Placement) {
    let prefs = Prefs {
        theme: theme.to_owned(),
        tabs: placement.into(),
    };
    let path = file();
    let text = match toml::to_string(&prefs) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("[prefs] cannot render {}: {e}", path.display());
            return;
        }
    };
    if let Some(dir) = path.parent()
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        eprintln!("[prefs] cannot create {}: {e}", dir.display());
        return;
    }
    if let Err(e) = std::fs::write(&path, text) {
        eprintln!("[prefs] cannot write {}: {e}", path.display());
    }
}

/// The themes the user dropped into `<state-dir>/themes`, by file stem.
///
/// Sorted by name: `read_dir` has no order of its own, and the palette lists
/// these after the pack.
pub fn themes() -> Vec<Theme> {
    let dir = zeughaus_link::credentials::state_dir().join(THEMES);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("[themes] cannot read {}: {e}", dir.display());
            return Vec::new();
        }
    };
    let mut themes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match Theme::from_wezterm(name, &text) {
                Ok(theme) => themes.push(theme),
                Err(e) => eprintln!("[themes] {}: {e} -- skipped", path.display()),
            },
            Err(e) => eprintln!("[themes] {}: {e} -- skipped", path.display()),
        }
    }
    themes.sort_by(|a, b| a.name().cmp(b.name()));
    themes
}

/// `zeughaus.toml`: what the user adds by hand.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Config {
    /// Runner URLs (`weida://sha256:...@host:port/`), each pinning its peer.
    remotes: Vec<String>,
}

/// The `remotes` of a `zeughaus.toml` text.
fn parse_remotes(text: &str) -> Result<Vec<String>, String> {
    toml::from_str::<Config>(text)
        .map(|config| config.remotes)
        .map_err(|e| e.to_string())
}

/// Modification time of `path`; `None` while it is missing.
fn stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Which files the runner list was read from, as of when.
///
/// The list is the runner of this machine (its `endpoint` file, rewritten
/// whenever that runner starts) followed by `zeughaus.toml`'s remotes. Both
/// change while the editor runs, so the editor asks [`Self::poll`] on its
/// clock instead of reading them again every turn.
pub struct RunnerSources {
    endpoint_mtime: Option<SystemTime>,
    config_mtime: Option<SystemTime>,
}

impl RunnerSources {
    /// The sources as they are now, with the runner URLs they name.
    pub fn load() -> (Self, Vec<String>) {
        let dir = zeughaus_link::credentials::state_dir();
        let sources = Self {
            endpoint_mtime: stamp(&zeughaus_link::credentials::endpoint_path(&dir)),
            config_mtime: stamp(&dir.join(CONFIG)),
        };
        (sources, read_runner_urls(&dir))
    }

    /// The new URL list when either file changed (or appeared or vanished)
    /// since the last call, else `None`.
    pub fn poll(&mut self) -> Option<Vec<String>> {
        let dir = zeughaus_link::credentials::state_dir();
        let endpoint_mtime = stamp(&zeughaus_link::credentials::endpoint_path(&dir));
        let config_mtime = stamp(&dir.join(CONFIG));
        if endpoint_mtime == self.endpoint_mtime && config_mtime == self.config_mtime {
            return None;
        }
        self.endpoint_mtime = endpoint_mtime;
        self.config_mtime = config_mtime;
        Some(read_runner_urls(&dir))
    }
}

/// The local runner's URL, then the remotes, each only if it can be dialled
/// with a pinned peer. A rejected entry is reported once per reload.
fn read_runner_urls(dir: &Path) -> Vec<String> {
    let mut candidates: Vec<String> = zeughaus_link::credentials::read_endpoint(dir)
        .into_iter()
        .collect();
    let path = dir.join(CONFIG);
    match std::fs::read_to_string(&path) {
        Ok(text) => match parse_remotes(&text) {
            Ok(remotes) => candidates.extend(remotes),
            Err(e) => eprintln!("[config] {}: {e}", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("[config] {}: {e}", path.display()),
    }
    candidates
        .into_iter()
        .filter(|url| match weida::EndpointAddr::parse(url) {
            Ok(addr) if addr.peer.is_some() => true,
            Ok(_) => {
                eprintln!("[config] skipping {url}: no pinned fingerprint");
                false
            }
            Err(e) => {
                eprintln!("[config] skipping {url}: {e}");
                false
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file is the contract between two runs of the editor and the hand
    /// that edits it: the placement is a word, not a Rust variant name, and a
    /// table naming one key keeps the default for the other.
    #[test]
    fn the_file_states_the_theme_and_the_placement_in_plain_words() {
        let written = toml::to_string(&Prefs {
            theme: "Nord".to_owned(),
            tabs: Tabs::Left,
        })
        .expect("prefs render");
        assert!(written.contains("theme = \"Nord\""), "{written}");
        assert!(written.contains("tabs = \"left\""), "{written}");

        let read: Prefs = toml::from_str(&written).expect("prefs parse");
        assert_eq!(read.theme, "Nord");
        assert!(matches!(Placement::from(read.tabs), Placement::Left));

        let partial: Prefs = toml::from_str("theme = \"Dracula\"").expect("partial parse");
        assert_eq!(partial.theme, "Dracula");
        assert!(matches!(Placement::from(partial.tabs), Placement::Top));
    }

    #[test]
    fn remotes_are_read_from_the_list_and_bad_toml_is_an_error() {
        let remotes = parse_remotes("remotes = [\"weida://a\", \"weida://b\"]").expect("parse");
        assert_eq!(remotes, ["weida://a", "weida://b"]);
        assert!(parse_remotes("").expect("empty").is_empty());
        assert!(parse_remotes("remotes = [").is_err());
    }
}
