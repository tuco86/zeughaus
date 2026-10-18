//! What `install` left on a host for the updater: `<state-dir>/install.toml`
//! names the profile the host was installed as, so a new build can rewrite
//! the same files, and the writes `install` and the refresh share.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `<state-dir>/install.toml`.
pub const INSTALL_FILE: &str = "install.toml";

/// How a host was installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallConfig {
    /// `desktop`, `macos`, `service`, `windows` or `ci`.
    pub profile: String,
    /// Where the binaries were put.
    pub bin_dir: PathBuf,
    /// The runner's `--feed-addr`, where the profile has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_addr: Option<String>,
}

/// `Ok(None)` when the host was not installed by `install`.
pub fn load(state_dir: &Path) -> Result<Option<InstallConfig>, String> {
    let path = state_dir.join(INSTALL_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `install.toml` and `update.toml` (following `channel`), replacing
/// what was there.
pub fn record(state_dir: &Path, config: &InstallConfig, channel: &str) -> Result<(), String> {
    if !crate::valid_channel(channel) {
        return Err(format!("channel `{channel}` must match [a-z0-9][a-z0-9-]*"));
    }
    let install = toml::to_string(config).map_err(|e| e.to_string())?;
    write_if_changed(&state_dir.join(INSTALL_FILE), install.as_bytes(), 0o644)?;
    let update = format!("channel = \"{channel}\"\n");
    write_if_changed(
        &state_dir.join(crate::CONFIG_FILE),
        update.as_bytes(),
        0o644,
    )?;
    Ok(())
}

/// The command that restarts the service a profile installed, for a message
/// that asks a person to run it.
pub fn restart_hint(profile: Option<&str>) -> &'static str {
    match profile {
        Some("desktop") => "systemctl --user reload zeughaus-runner",
        Some("service") => "sudo systemctl --user -M zeughaus@ reload zeughaus-runner",
        Some("macos") => "launchctl kill SIGUSR1 gui/$(id -u)/net.doodleshnookie.zeughaus-runner",
        Some("windows") => "schtasks /End /TN zeughaus-runner && schtasks /Run /TN zeughaus-runner",
        Some("ci") => "sudo systemctl --user -M zeughaus-ci@ restart zeughaus-ci",
        _ => "restart it",
    }
}

/// Writes `bytes` to `path` unless it already holds them: parents created,
/// a temporary file renamed into place, `mode` set on unix. Answers whether
/// it wrote.
pub fn write_if_changed(path: &Path, bytes: &[u8], mode: u32) -> Result<bool, String> {
    let same = std::fs::read(path).is_ok_and(|old| old == bytes);
    if !same {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let tmp = crate::replace::sibling(path, ".tmp");
        std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    set_mode(path, mode)?;
    Ok(!same)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let current = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
        .permissions()
        .mode();
    if current & 0o7777 != mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("cannot chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) -> Result<(), String> {
    Ok(())
}
