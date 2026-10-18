//! What `install` puts on a host and leaves for the updater.
//!
//! A release archive carries the files a host needs besides the binaries
//! (units, desktop entry, VM scripts) under `share/`, at their paths in the
//! repository. `install <profile>` writes them where the profile wants them
//! and records the profile in `<state-dir>/install.toml`, so every new build
//! the updater installs can write them again from its own release.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::Product;
use crate::replace::{Replacement, replace_all};

/// One file of a release's `share/` and where a profile puts it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStep {
    /// The path in the repository, and under `share/`.
    pub share: &'static str,
    pub to: PathBuf,
    /// Unix permission bits.
    pub mode: u32,
}

/// A [`FileStep`] read and rendered, ready to be written.
pub struct Rendered {
    pub to: PathBuf,
    pub bytes: Vec<u8>,
    pub mode: u32,
}

/// Reads every step's file from the unpacked release at `from` and replaces
/// each `(placeholder, value)` in it. Nothing is written: a release that
/// lacks a file is refused before anything changes.
pub fn render_files(
    from: &Path,
    steps: &[FileStep],
    values: &[(&str, &str)],
) -> Result<Vec<Rendered>, String> {
    steps
        .iter()
        .map(|step| {
            let path = from.join("share").join(step.share);
            let text = std::fs::read_to_string(&path)
                .map_err(|_| format!("the release has no share/{}", step.share))?;
            Ok(Rendered {
                to: step.to.clone(),
                bytes: render(&text, values).into_bytes(),
                mode: step.mode,
            })
        })
        .collect()
}

/// `text` with every placeholder replaced by its value.
pub fn render(text: &str, values: &[(&str, &str)]) -> String {
    values
        .iter()
        .fold(text.to_owned(), |text, (placeholder, value)| {
            text.replace(placeholder, value)
        })
}

/// Writes what [`render_files`] made; answers the paths that changed.
pub fn write_rendered(files: &[Rendered]) -> Result<Vec<PathBuf>, String> {
    let mut changed = Vec::new();
    for file in files {
        if write_if_changed(&file.to, &file.bytes, file.mode)? {
            changed.push(file.to.clone());
        }
    }
    Ok(changed)
}

/// Copies the binary `from` over `to` by a rename, keeping the one it
/// replaces as `<to>.previous`.
pub fn install_binary(from: &Path, to: &Path) -> Result<(), String> {
    replace_all(&[Replacement::new(
        from.to_path_buf(),
        to.to_path_buf(),
        false,
    )])
}

/// The same for an app bundle (a directory, copied with `ditto`).
pub fn install_bundle(from: &Path, to: &Path) -> Result<(), String> {
    replace_all(&[Replacement::new(from.to_path_buf(), to.to_path_buf(), true)])
}

/// The build the release at `from` was published as, from what its binary
/// answers to `version`.
pub fn release_build(from: &Path, product: Product) -> Result<u64, String> {
    let binary = from.join(product.file_name());
    let output = Command::new(&binary)
        .arg("version")
        .output()
        .map_err(|e| format!("cannot run {}: {e}", binary.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .rsplit_once(" build ")
        .and_then(|(_, build)| build.parse().ok())
        .filter(|_| output.status.success())
        .ok_or_else(|| format!("{} version: {}", binary.display(), text.trim()))
}

/// `systemctl --user <args>`.
pub fn systemctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("cannot run systemctl: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl --user {}: {}",
            args.join(" "),
            crate::stderr_of(&output)
        ))
    }
}

/// Enables `unit` of the user's manager and `verb`s it (`reload` or
/// `restart`) when it runs, else starts it. Answers what it did.
pub fn start_or(unit: &str, verb: &str) -> Result<&'static str, String> {
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", unit])?;
    if systemctl(&["is-active", "--quiet", unit]).is_ok() {
        systemctl(&[verb, unit])?;
        Ok(if verb == "reload" {
            "reloaded"
        } else {
            "restarted"
        })
    } else {
        systemctl(&["start", unit])?;
        Ok("started")
    }
}

/// `$HOME`, which every profile writes under.
pub fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| "HOME is not set to an absolute path".to_owned())
}

/// `$XDG_CONFIG_HOME`, else `~/.config`.
pub fn config_home(home: &Path) -> PathBuf {
    xdg("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
}

/// `$XDG_DATA_HOME`, else `~/.local/share`.
pub fn data_home(home: &Path) -> PathBuf {
    xdg("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share"))
}

/// An XDG directory variable; the spec ignores one that is not absolute.
fn xdg(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

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
