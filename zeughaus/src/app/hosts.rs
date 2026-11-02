//! Which machine a tab, a group or a card runs on, drawn as a glyph: the
//! device a CI's runner labels say it is. The local runner draws none.

use super::App;
use crate::workspace::RunnerKey;

/// The glyph of a runner's `device=` label (material icons of the bundled
/// Propo face); the codicon of a remote machine without one.
pub(super) fn device_glyph(labels: &[String]) -> char {
    labels
        .iter()
        .find_map(|label| match label.as_str() {
            "device=laptop" => Some('\u{f0322}'),
            "device=desktop" => Some('\u{f0379}'),
            "device=media" => Some('\u{f0502}'),
            "device=server" => Some('\u{f048b}'),
            _ => None,
        })
        .unwrap_or('\u{eb3a}')
}

/// The glyph of graphs whose runner is not connected.
pub(super) const OFFLINE_GLYPH: char = '\u{ead0}';

impl App {
    /// The device glyph of runner `key`, this machine's included.
    pub(super) fn device_glyph_of(&self, key: &RunnerKey) -> char {
        device_glyph(self.labels_of(key))
    }

    /// What the CIs list as runner `key`'s labels.
    fn labels_of(&self, key: &RunnerKey) -> &[String] {
        self.directory
            .iter()
            .find(|runner| runner.fingerprint == key.as_str())
            .map_or(&[][..], |runner| runner.labels.as_slice())
    }

    /// The glyph and name of the machine behind section `key`: `None` for
    /// this machine's own runner and for the editor's local graphs.
    pub(super) fn host_of(&self, key: &RunnerKey) -> Option<(char, String)> {
        match key.as_str() {
            RunnerKey::LOCAL => return None,
            RunnerKey::OFFLINE => return Some((OFFLINE_GLYPH, "not running".to_owned())),
            _ => {}
        }
        if self.local_runner.as_ref() == Some(key) {
            return None;
        }
        let name = self
            .runtime
            .links
            .get(key)
            .map(|link| link.label.clone())
            .unwrap_or_default();
        Some((self.device_glyph_of(key), name))
    }
}
