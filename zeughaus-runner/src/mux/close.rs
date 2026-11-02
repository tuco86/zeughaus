//! Whether a terminal whose child exited closes by itself.
//!
//! `close_on_exit` in `<state-dir>/zeughaus.toml`: `always` (the default),
//! `success` (only after exit code 0) or `never`. It applies to shells and
//! agent sessions; a job's run closes when its wait ends, and a replay never
//! closes by itself. The file is read at each decision, so a change applies
//! to the next exit without a restart.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use zeughaus_mux::ExitState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOnExit {
    Always,
    Success,
    Never,
}

#[derive(Deserialize)]
struct File {
    #[serde(default)]
    close_on_exit: Option<String>,
}

/// The policy `<state-dir>/zeughaus.toml` names; [`CloseOnExit::Always`]
/// without the file, the key, or with a value it does not know.
pub fn policy(state_dir: &Path) -> CloseOnExit {
    let text = std::fs::read_to_string(state_dir.join("zeughaus.toml")).unwrap_or_default();
    parse(&text)
}

fn parse(text: &str) -> CloseOnExit {
    static WARNED: AtomicBool = AtomicBool::new(false);
    let value = toml::from_str::<File>(text)
        .ok()
        .and_then(|file| file.close_on_exit);
    match value.as_deref() {
        None | Some("always") => CloseOnExit::Always,
        Some("success") => CloseOnExit::Success,
        Some("never") => CloseOnExit::Never,
        Some(_) => {
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!("[mux] zeughaus.toml: close_on_exit must be always, success or never");
            }
            CloseOnExit::Always
        }
    }
}

/// Whether a terminal that exited with `exit` closes now: one without a run
/// of its own (a job's run is closed by its wait) that is no replay, as the
/// policy says.
pub fn closes(policy: CloseOnExit, exit: &ExitState, has_run: bool, replay: bool) -> bool {
    if has_run || replay {
        return false;
    }
    match policy {
        CloseOnExit::Always => true,
        CloseOnExit::Success => matches!(exit, ExitState::Exited { code: 0 }),
        CloseOnExit::Never => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_policy_defaults_to_always() {
        assert_eq!(parse(""), CloseOnExit::Always);
        assert_eq!(parse("ci = [\"weida://x/\"]\n"), CloseOnExit::Always);
        assert_eq!(parse("close_on_exit = \"success\"\n"), CloseOnExit::Success);
        assert_eq!(parse("close_on_exit = \"never\"\n"), CloseOnExit::Never);
        assert_eq!(
            parse("close_on_exit = \"sometimes\"\n"),
            CloseOnExit::Always
        );
        assert_eq!(parse("close_on_exit = 3\n"), CloseOnExit::Always);
    }

    #[test]
    fn only_shells_and_sessions_close_as_the_policy_says() {
        let ok = ExitState::Exited { code: 0 };
        let failed = ExitState::Exited { code: 2 };
        assert!(closes(CloseOnExit::Always, &failed, false, false));
        assert!(closes(CloseOnExit::Success, &ok, false, false));
        assert!(!closes(CloseOnExit::Success, &failed, false, false));
        assert!(!closes(
            CloseOnExit::Success,
            &ExitState::Killed,
            false,
            false
        ));
        assert!(!closes(CloseOnExit::Never, &ok, false, false));
        assert!(!closes(CloseOnExit::Always, &ok, true, false));
        assert!(!closes(CloseOnExit::Always, &ok, false, true));
    }
}
