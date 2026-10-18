//! `<state-dir>/updates/state.json`: which build the updater installed, the
//! one it replaced, and whether the new one has come up.
//!
//! A build that was installed starts unconfirmed. Every start of it counts
//! ([`boot_action`]); one that has run [`crate::CONFIRM_AFTER`] is confirmed,
//! and the fourth start of one that never was puts the previous build back.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The starts an unconfirmed build gets before the next one rolls it back.
pub const STARTS: u32 = 3;

/// Unknown fields are kept out of the way, not refused: a later build may
/// add one and an older one must still read the file after a rollback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// The build the updater put in place last.
    pub installed: u64,
    /// The build that one replaced, kept as `<target>.previous`; 0 for none.
    pub previous: u64,
    /// Starts of `installed` since it was put in place.
    pub starts: u32,
    /// Whether `installed` has run long enough to be trusted.
    pub confirmed: bool,
    /// The build whose install files were last written from its release.
    pub refreshed: u64,
    /// A build that was rolled back and is not installed again.
    pub refused: Option<u64>,
}

/// What a starting process does about the state it finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootAction {
    /// Not a build on trial: nothing to count.
    Nothing,
    /// Another start of an unconfirmed build.
    Count,
    /// The unconfirmed build has failed to come up [`STARTS`] times.
    RollBack,
}

pub fn boot_action(state: Option<&State>, own: u64) -> BootAction {
    match state {
        _ if own == 0 => BootAction::Nothing,
        None => BootAction::Nothing,
        Some(state) if state.installed != own || state.confirmed => BootAction::Nothing,
        Some(state) if state.starts < STARTS => BootAction::Count,
        Some(_) => BootAction::RollBack,
    }
}

/// Serializes the read-modify-write cycles of one process: the boot check,
/// the confirmation and an install never interleave.
static LOCK: Mutex<()> = Mutex::new(());

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("updates").join("state.json")
}

/// `Ok(None)` when there is no file: nothing was ever installed here.
pub fn load(state_dir: &Path) -> Result<Option<State>, String> {
    let path = path(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `state` by a rename, so a reader never sees half of it.
pub fn save(state_dir: &Path, state: &State) -> Result<(), String> {
    let path = path(state_dir);
    let dir = path.parent().expect("state.json has a directory");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string(state).expect("the state serializes");
    std::fs::write(&tmp, json).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Loads the state (a default one when there is none), lets `change` edit it
/// and writes it when `change` says it did. Answers what `change` answered.
pub fn modify<T>(
    state_dir: &Path,
    change: impl FnOnce(&mut State) -> (bool, T),
) -> Result<T, String> {
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = load(state_dir)?.unwrap_or_default();
    let (changed, answer) = change(&mut state);
    if changed {
        save(state_dir, &state)?;
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial(starts: u32) -> State {
        State {
            installed: 7,
            previous: 3,
            starts,
            ..State::default()
        }
    }

    #[test]
    fn only_an_unconfirmed_installed_build_counts_its_starts() {
        // A build the CI did not make is never on trial.
        assert_eq!(boot_action(Some(&trial(3)), 0), BootAction::Nothing);
        assert_eq!(boot_action(None, 7), BootAction::Nothing);
        // A build that was not the last one installed, e.g. after a rollback.
        assert_eq!(boot_action(Some(&trial(3)), 3), BootAction::Nothing);
        let confirmed = State {
            confirmed: true,
            ..trial(3)
        };
        assert_eq!(boot_action(Some(&confirmed), 7), BootAction::Nothing);
    }

    #[test]
    fn the_fourth_unconfirmed_start_rolls_back() {
        for starts in 0..STARTS {
            assert_eq!(boot_action(Some(&trial(starts)), 7), BootAction::Count);
        }
        assert_eq!(boot_action(Some(&trial(STARTS)), 7), BootAction::RollBack);
        assert_eq!(
            boot_action(Some(&trial(STARTS + 5)), 7),
            BootAction::RollBack
        );
    }

    #[test]
    fn a_state_file_with_fields_it_does_not_know_still_reads() {
        let state: State =
            serde_json::from_str(r#"{"installed": 9, "refused": 8, "later": [1]}"#).unwrap();
        assert_eq!(state.installed, 9);
        assert_eq!(state.refused, Some(8));
        assert_eq!(state.previous, 0);
        assert!(!state.confirmed);
    }
}
