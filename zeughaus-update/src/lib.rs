//! The self-updater: a process that follows a release channel and installs
//! what the CI publishes there. The runner and `zeughaus-ci` both use it,
//! each for its own binary ([`Product`]).
//!
//! `<state-dir>/update.toml` turns it on; without the file nothing is
//! downloaded:
//!
//! ```toml
//! channel = "dev"
//! url = "https://zeughaus.doodleshnookie.net/updates"   # the default
//! ```
//!
//! One thread reads `<url>/<channel>/latest`, a CI build number, once a
//! minute. A build newer than this binary's own ([`manifest::own_build`]) is
//! fetched into `<state-dir>/updates/<build>/`: its manifest first, checked
//! against the keys compiled in ([`manifest::TRUSTED_KEYS`]) before any other
//! byte of the release is believed, then this host's archive, checked against
//! the size and SHA-256 the manifest gives. The new binary must then answer
//! `version` with the build it was published as, which catches one that
//! cannot load or panics at once. Only then the binary is replaced by a
//! rename of a file written next to it, the replaced one kept as
//! `<binary>.previous`. A runner also replaces the editor installed beside it
//! (`zeughaus` in the runner's directory, or `~/Applications/Zeughaus.app`
//! on macOS), so the two always run one build, and asks running editors to
//! restart by `SIGUSR1`. The process then restarts the way the caller's
//! `request_restart` makes it.
//!
//! A build that was installed is on trial (`updates/state.json`): [`boot`]
//! counts its starts, it is confirmed once it has run [`CONFIRM_AFTER`], and
//! its fourth start without that puts the previous build back and refuses
//! the build until a newer one is published. `update rollback` ([`command`])
//! does the same by hand.

pub mod editor;
pub mod host;
pub mod manifest;
mod replace;
mod state;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::editor::Editor;
use crate::replace::{Replacement, previous_of, replace_all};
use crate::state::{BootAction, STARTS};

/// `<state-dir>/update.toml`.
pub const CONFIG_FILE: &str = "update.toml";

/// Where the zeughaus CI publishes its channels.
const DEFAULT_URL: &str = "https://zeughaus.doodleshnookie.net/updates";

/// The wait before the first look, so that a process that has just started,
/// or restarted into an update, is serving before it begins a download.
const FIRST_LOOK: Duration = Duration::from_secs(10);

/// The time between two looks at the channel.
const INTERVAL: Duration = Duration::from_secs(60);

/// How long an installed build runs before it is trusted. A build that is
/// started again before that has not come up.
pub const CONFIRM_AFTER: Duration = Duration::from_secs(120);

/// What the new binary's `version` may take.
const PREFLIGHT_LIMIT: Duration = Duration::from_secs(30);

/// What the `latest` file, a few bytes, may take.
const LATEST_SECONDS: &str = "20";

/// What a manifest or an archive may take; the archive is a whole build.
const DOWNLOAD_SECONDS: &str = "600";

/// The binary an updater replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    /// `zeughaus-runner`, and the editor installed beside it.
    Runner,
    /// `zeughaus-ci` alone.
    Ci,
}

impl Product {
    /// The binary's name without the platform's suffix.
    pub fn binary(self) -> &'static str {
        match self {
            Product::Runner => "zeughaus-runner",
            Product::Ci => "zeughaus-ci",
        }
    }

    /// The binary's file name in an archive and on disk.
    pub fn file_name(self) -> String {
        format!("{}{}", self.binary(), std::env::consts::EXE_SUFFIX)
    }
}

/// `[a-z0-9][a-z0-9-]*`: a channel names a directory under the release URL.
fn valid_channel(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn default_url() -> String {
    DEFAULT_URL.to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateConfig {
    /// The release channel followed: a directory under `url`.
    channel: String,
    /// Where the channels are published.
    #[serde(default = "default_url")]
    url: String,
}

impl UpdateConfig {
    fn parse(text: &str) -> Result<UpdateConfig, String> {
        let mut config: UpdateConfig = toml::from_str(text).map_err(|e| e.to_string())?;
        if !valid_channel(&config.channel) {
            return Err(format!(
                "channel `{}` must match [a-z0-9][a-z0-9-]*",
                config.channel
            ));
        }
        if !(config.url.starts_with("https://") || config.url.starts_with("http://")) {
            return Err(format!(
                "url `{}` must start with https:// or http://",
                config.url
            ));
        }
        // Every URL below is `<url>/<channel>/..`.
        config.url.truncate(config.url.trim_end_matches('/').len());
        Ok(config)
    }
}

/// `Ok(None)` when there is no `update.toml`: this host does not update.
/// An error names the file.
fn load(state_dir: &Path) -> Result<Option<UpdateConfig>, String> {
    let path = state_dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    UpdateConfig::parse(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Starts the updater's thread. It confirms a build on trial once it has run
/// [`CONFIRM_AFTER`], with or without `update.toml`, and follows the channel
/// that file names.
///
/// `exe` is this binary's path as resolved at start, the file an update
/// replaces. `may_restart` says whether an update may begin now (it ends in
/// a restart); `request_restart` asks the process to restart into the build
/// just installed.
pub fn start(
    state_dir: &Path,
    exe: PathBuf,
    product: Product,
    may_restart: Box<dyn Fn() -> bool + Send>,
    request_restart: Box<dyn Fn() + Send>,
) {
    let started = Instant::now();
    let own = manifest::own_build();
    let config = load(state_dir).unwrap_or_else(|e| {
        eprintln!("[update] {e}");
        None
    });
    let updater = config.and_then(|config| {
        let Some(platform) = manifest::host_platform() else {
            eprintln!(
                "[update] no release is built for {} on {}; not updating",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            return None;
        };
        eprintln!(
            "[update] following {} at {}, running build {own}",
            config.channel, config.url
        );
        Some(Updater {
            config,
            platform,
            work: state_dir.join("updates"),
            state_dir: state_dir.to_path_buf(),
            exe,
            product,
            home: home(),
            keys: manifest::TRUSTED_KEYS.to_vec(),
            own,
            may_restart,
            request_restart,
        })
    });
    let state_dir = state_dir.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("zeughaus-update".into())
        .spawn(move || {
            // A build on trial is confirmed before anything replaces it: the
            // previous build kept for its rollback must be one that ran.
            let trial = matches!(
                state::load(&state_dir),
                Ok(Some(state)) if own != 0 && state.installed == own && !state.confirmed
            );
            if trial {
                std::thread::sleep(CONFIRM_AFTER.saturating_sub(started.elapsed()));
                confirm(&state_dir, own);
            }
            if let Some(updater) = updater {
                if !trial {
                    std::thread::sleep(FIRST_LOOK);
                }
                updater.run();
            }
        });
    if let Err(e) = spawned {
        eprintln!("[update] cannot start: {e}");
    }
}

/// Marks `own` as come up, if it is the build on trial.
fn confirm(state_dir: &Path, own: u64) {
    let confirmed = state::modify(state_dir, |state| {
        let now = state.installed == own && !state.confirmed;
        state.confirmed |= now;
        (now, now)
    });
    match confirmed {
        Ok(true) => eprintln!("[update] build {own} confirmed"),
        Ok(false) => {}
        Err(e) => eprintln!("[update] {e}"),
    }
}

/// The check a process makes once at start, before it serves: another start
/// of a build on trial is counted, and the fourth puts the previous build
/// back and starts that one in place of this process (unix), or ends this
/// process for its supervisor to start it (Windows).
pub fn boot(state_dir: &Path, exe: &Path, product: Product) {
    match boot_restore(
        state_dir,
        exe,
        product,
        home().as_deref(),
        manifest::own_build(),
    ) {
        Ok(false) => {}
        Ok(true) => reexec(exe),
        Err(e) => eprintln!("[update] {e}"),
    }
}

/// [`boot`] without the exec: `Ok(true)` when the previous build was put
/// back and must be started.
fn boot_restore(
    state_dir: &Path,
    exe: &Path,
    product: Product,
    home: Option<&Path>,
    own: u64,
) -> Result<bool, String> {
    let found = state::load(state_dir)?;
    match state::boot_action(found.as_ref(), own) {
        BootAction::Nothing => Ok(false),
        BootAction::Count => {
            state::modify(state_dir, |state| {
                state.starts += 1;
                (true, ())
            })?;
            Ok(false)
        }
        BootAction::RollBack => {
            if !previous_of(exe).exists() {
                eprintln!(
                    "[update] build {own} did not come up {STARTS} times and there is no \
                     previous build"
                );
                return Ok(false);
            }
            let back = roll_back(state_dir, exe, product, home, own)?;
            eprintln!(
                "[update] build {} did not come up {STARTS} times; back to build {}",
                back.from, back.to
            );
            if let Some(editor) = &back.editor {
                editor.restart(back.from);
            }
            Ok(true)
        }
    }
}

/// What a rollback put back.
struct RolledBack {
    /// The build that was replaced, now refused.
    from: u64,
    /// The build that is in place again.
    to: u64,
    /// The editor that went back with the runner, to be restarted.
    editor: Option<Editor>,
}

/// Puts every `<target>.previous` back and records it: the build that was
/// installed is refused, the previous one is installed and confirmed.
fn roll_back(
    state_dir: &Path,
    exe: &Path,
    product: Product,
    home: Option<&Path>,
    own: u64,
) -> Result<RolledBack, String> {
    if !previous_of(exe).exists() {
        return Err("no previous build to go back to".into());
    }
    replace::restore(exe, false)?;
    let editor = match product {
        Product::Runner => Editor::find(exe, home),
        Product::Ci => None,
    };
    if let Some(editor) = &editor
        && let Err(e) = replace::restore(editor.target(), editor.is_bundle())
    {
        eprintln!("[update] {e}");
    }
    state::modify(state_dir, |state| {
        let from = if state.installed != 0 {
            state.installed
        } else {
            own
        };
        let to = state.previous;
        state.refused = (from != 0).then_some(from);
        state.installed = to;
        state.previous = 0;
        state.confirmed = true;
        state.starts = 0;
        (true, RolledBack { from, to, editor })
    })
}

/// Replaces this process with `exe`, same arguments; where that cannot be
/// done, ends it for the supervisor to start `exe` again.
fn reexec(exe: &Path) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = Command::new(exe).args(std::env::args_os().skip(1)).exec();
        eprintln!("[update] cannot start {}: {e}", exe.display());
    }
    #[cfg(not(unix))]
    let _ = exe;
    std::process::exit(1)
}

/// `update status` and `update rollback`: the answer to print, or why not.
pub fn command(
    args: &[String],
    state_dir: &Path,
    exe: &Path,
    product: Product,
) -> Result<String, String> {
    match args {
        [command] if command == "status" => Ok(status(state_dir, manifest::own_build())),
        [command] if command == "rollback" => {
            let back = roll_back(
                state_dir,
                exe,
                product,
                home().as_deref(),
                manifest::own_build(),
            )?;
            if let Some(editor) = &back.editor {
                editor.restart(back.from);
            }
            let profile = host::load(state_dir).ok().flatten().map(|c| c.profile);
            Ok(format!(
                "rolled back to build {}; restart the service to run it: {}",
                back.to,
                host::restart_hint(profile.as_deref())
            ))
        }
        _ => Err("usage: update status|rollback".into()),
    }
}

fn status(state_dir: &Path, own: u64) -> String {
    let following = match load(state_dir) {
        Ok(Some(config)) => format!("following {} at {}", config.channel, config.url),
        Ok(None) => format!("not following a channel (no {CONFIG_FILE})"),
        Err(e) => e,
    };
    let dash = |n: u64| {
        if n == 0 {
            "-".to_owned()
        } else {
            n.to_string()
        }
    };
    let (trial, refused) = match state::load(state_dir) {
        Ok(state) => {
            let state = state.unwrap_or_default();
            let trust = if state.confirmed {
                "confirmed".to_owned()
            } else {
                format!("unconfirmed, {} starts", state.starts)
            };
            (
                format!(
                    "installed {}, previous {}, {trust}",
                    dash(state.installed),
                    dash(state.previous)
                ),
                format!("refused {}", state.refused.map_or_else(|| "-".into(), dash)),
            )
        }
        Err(e) => (e, "refused -".to_owned()),
    };
    format!(
        "{following}\nrunning build {} ({})\n{trial}\n{refused}",
        dash(own),
        env!("CARGO_PKG_VERSION")
    )
}

struct Updater {
    config: UpdateConfig,
    /// What the release calls this host.
    platform: &'static str,
    /// `<state-dir>/updates`: one directory per build being installed.
    work: PathBuf,
    state_dir: PathBuf,
    /// This process's own binary, the file an update replaces.
    exe: PathBuf,
    product: Product,
    /// Where `Zeughaus.app` is looked for on macOS.
    home: Option<PathBuf>,
    /// The keys a manifest may be signed by.
    keys: Vec<[u8; 32]>,
    /// The build this binary was made by; 0 when the CI did not make it.
    own: u64,
    may_restart: Box<dyn Fn() -> bool + Send>,
    request_restart: Box<dyn Fn() + Send>,
}

impl Updater {
    fn run(self) {
        // The build the last attempt was for, installed or not: a restart
        // that fails leaves this image running at its old build number, and
        // a build that failed would fail the same way a minute later.
        let mut attempted = None;
        // Said once per outage, not once a minute.
        let mut unreachable = false;
        loop {
            self.look(&mut attempted, &mut unreachable);
            std::thread::sleep(INTERVAL);
        }
    }

    /// One look at the channel, and the attempt at a build it calls for.
    fn look(&self, attempted: &mut Option<u64>, unreachable: &mut bool) {
        let latest = match self.latest() {
            Ok(latest) => latest,
            Err(e) => {
                if !*unreachable {
                    eprintln!("[update] {e}");
                }
                *unreachable = true;
                return;
            }
        };
        *unreachable = false;
        if latest <= self.own || *attempted == Some(latest) {
            return;
        }
        // A build rolled back here stays out until a newer one is published.
        let refused = state::load(&self.state_dir)
            .ok()
            .flatten()
            .and_then(|s| s.refused);
        if refused == Some(latest) || !(self.may_restart)() {
            return;
        }
        *attempted = Some(latest);
        self.update(latest);
    }

    /// The newest build of the channel.
    fn latest(&self) -> Result<u64, String> {
        let url = format!("{}/{}/latest", self.config.url, self.config.channel);
        let text = fetch(&url)?;
        parse_build(&text).ok_or_else(|| {
            let shown: String = text.trim().chars().take(40).collect();
            format!("{url}: expected a build number, got {shown:?}")
        })
    }

    /// One attempt at `build`, to its end: installed and the restarts asked
    /// for, or logged and removed.
    fn update(&self, build: u64) {
        match self.install(build) {
            Ok(installed) => {
                let own = self.own;
                let recorded = state::modify(&self.state_dir, |state| {
                    state.installed = build;
                    state.previous = own;
                    state.starts = 0;
                    state.confirmed = false;
                    (true, ())
                });
                if let Err(e) = recorded {
                    eprintln!("[update] {e}");
                }
                self.prune(&[build, own]);
                eprintln!(
                    "[update] installed build {build} ({}); restarting",
                    installed.version
                );
                if let Some(editor) = &installed.editor {
                    editor.restart(own);
                }
                (self.request_restart)();
            }
            Err(e) => {
                eprintln!("[update] build {build}: {e}");
                let _ = std::fs::remove_dir_all(self.work.join(build.to_string()));
            }
        }
    }

    /// Downloads, checks and installs `build`. Nothing outside `updates/`
    /// changes until every check has passed.
    fn install(&self, build: u64) -> Result<Installed, String> {
        let dir = self.work.join(build.to_string());
        // What an attempt cut short by a restart left.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let base = format!("{}/{}/{build}", self.config.url, self.config.channel);

        let json_path = dir.join(manifest::MANIFEST_FILE);
        let signature_path = dir.join(manifest::SIGNATURE_FILE);
        download(&format!("{base}/{}", manifest::MANIFEST_FILE), &json_path)?;
        download(
            &format!("{base}/{}", manifest::SIGNATURE_FILE),
            &signature_path,
        )?;
        let json = read(&json_path)?;
        let signature = read(&signature_path)?;
        manifest::verify_with(&self.keys, &json, &signature)?;
        let manifest = manifest::parse(&json)?;
        // The signature binds the build number: an old release served as a
        // new one is refused here.
        if manifest.build != build {
            return Err(format!("the manifest is for build {}", manifest.build));
        }
        if manifest.channel != self.config.channel {
            return Err(format!(
                "the manifest is for channel {}, not {}",
                manifest.channel, self.config.channel
            ));
        }
        let artifact = manifest
            .artifacts
            .iter()
            .find(|artifact| artifact.platform == self.platform)
            .ok_or_else(|| format!("the release has no archive for {}", self.platform))?;
        let archive = dir.join(&artifact.file);
        download(&format!("{base}/{}", artifact.file), &archive)?;
        artifact.check(&archive)?;

        let staging = dir.join("staging");
        extract(&archive, &staging)?;
        let stem = manifest::stem(&manifest.version, self.platform);
        let top = staging.join(&stem);
        if !top.is_dir() {
            return Err(format!("the archive has no {stem}/ directory"));
        }
        let binary = top.join(self.product.file_name());
        preflight(
            &binary,
            self.product.binary(),
            &format!("{} build {build}", manifest.version),
            &dir,
        )?;

        let editor = match self.product {
            Product::Runner => Editor::find(&self.exe, self.home.as_deref()),
            Product::Ci => None,
        };
        let mut replacements = vec![Replacement::new(binary, self.exe.clone(), false)];
        if let Some(editor) = &editor {
            replacements.push(editor.replacement(&top));
        }
        replace_all(&replacements)?;
        Ok(Installed {
            version: manifest.version,
            editor,
        })
    }

    /// Removes every directory under `updates/` but those of `keep`: the
    /// builds that failed and the ones that were replaced. Files stay.
    fn prune(&self, keep: &[u64]) {
        let keep: Vec<String> = keep.iter().map(u64::to_string).collect();
        let Ok(entries) = std::fs::read_dir(&self.work) else {
            return;
        };
        for entry in entries.flatten() {
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let kept = keep.iter().any(|name| entry.file_name() == name.as_str());
            if is_dir && !kept {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}

/// What an update put on disk.
struct Installed {
    version: String,
    /// The editor that was replaced along with the runner, to be restarted.
    editor: Option<Editor>,
}

/// The build number a `latest` file holds.
fn parse_build(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Runs `<binary> version` and wants `expected` back within
/// [`PREFLIGHT_LIMIT`]. Its output goes to files under `scratch`, so a
/// binary that writes much cannot block on a full pipe.
fn preflight(binary: &Path, name: &str, expected: &str, scratch: &Path) -> Result<(), String> {
    let fail = |detail: String| format!("the new {name} does not start: {detail}");
    let out_path = scratch.join("preflight.out");
    let err_path = scratch.join("preflight.err");
    let create = |path: &Path| {
        std::fs::File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))
    };
    let mut child = Command::new(binary)
        .arg("version")
        .stdin(Stdio::null())
        .stdout(create(&out_path)?)
        .stderr(create(&err_path)?)
        .spawn()
        .map_err(|e| fail(e.to_string()))?;
    let deadline = Instant::now() + PREFLIGHT_LIMIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!(
                    "timed out after {} s",
                    PREFLIGHT_LIMIT.as_secs()
                )));
            }
            Err(e) => return Err(fail(e.to_string())),
        }
    };
    let stdout = String::from_utf8_lossy(&std::fs::read(&out_path).unwrap_or_default())
        .trim()
        .to_owned();
    if status.success() && stdout == expected {
        return Ok(());
    }
    let mut stderr = std::fs::read(&err_path).unwrap_or_default();
    stderr.truncate(200);
    let stderr = String::from_utf8_lossy(&stderr);
    let stderr = stderr.trim();
    Err(fail(if !status.success() {
        format!("{status}: {stderr}")
    } else {
        format!("it reports {stdout:?}, not {expected:?}")
    }))
}

/// The body of `url`, which is small.
fn fetch(url: &str) -> Result<String, String> {
    let output = Command::new("curl")
        .args(["-fsS", "--max-time", LATEST_SECONDS, url])
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !output.status.success() {
        return Err(format!("{url}: {}", stderr_of(&output)));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("{url}: the answer is not text"))
}

/// `url` into the file `to`.
fn download(url: &str, to: &Path) -> Result<(), String> {
    let output = Command::new("curl")
        .args(["-fsSL", "--max-time", DOWNLOAD_SECONDS, "-o"])
        .arg(to)
        .arg(url)
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("{url}: {}", stderr_of(&output)))
    }
}

/// Unpacks `archive` into `into`. `tar` finds out what the archive is: GNU
/// tar and bsdtar read a gzipped tar, and Windows' `tar.exe` (bsdtar) a zip.
fn extract(archive: &Path, into: &Path) -> Result<(), String> {
    std::fs::create_dir_all(into).map_err(|e| format!("cannot create {}: {e}", into.display()))?;
    let output = Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|e| format!("cannot run tar: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!("tar: {}", stderr_of(&output)))
    }
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

fn stderr_of(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let text = text.trim();
    if text.is_empty() {
        format!("{}", output.status)
    } else {
        text.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_file_holds_a_build_number() {
        assert_eq!(parse_build("1234\n"), Some(1234));
        assert_eq!(parse_build("  7 "), Some(7));
        for bad in ["", "dev", "-1", "1.5", "<html>", "12 13"] {
            assert_eq!(parse_build(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_config_defaults_its_url() {
        let config = UpdateConfig::parse("channel = \"dev\"").unwrap();
        assert_eq!(config.channel, "dev");
        assert_eq!(config.url, DEFAULT_URL);

        let config =
            UpdateConfig::parse("channel = \"nightly\"\nurl = \"http://localhost:8080/updates/\"")
                .unwrap();
        assert_eq!(config.url, "http://localhost:8080/updates");
    }

    #[test]
    fn the_config_refuses_what_it_does_not_know() {
        for bad in [
            "",
            "url = \"https://example.org\"",
            "channel = \"Dev\"",
            "channel = \"a/b\"",
            "channel = \"dev\"\nurl = \"ftp://example.org\"",
            "channel = \"dev\"\nurl = \"-o/etc/passwd\"",
            "channel = \"dev\"\nmirror = \"x\"",
            "channel = 3",
        ] {
            assert!(UpdateConfig::parse(bad).is_err(), "{bad:?}");
        }
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`). The fixture's binaries are
    /// shell scripts, so unix only.
    #[cfg(unix)]
    mod io {
        use std::os::unix::fs::PermissionsExt;

        use super::*;

        use crate::manifest::{Artifact, Manifest};
        use crate::replace::{replace_aside, restore_aside};
        use crate::state::State;

        const SEED: [u8; 32] = [7; 32];

        const OTHER: [u8; 32] = [9; 32];

        const CHANNEL: &str = "dev";

        const PLATFORM: &str = "linux-amd64";

        const VERSION: &str = "1.2.3";

        /// A site, a runner, an editor and a state directory in one temporary
        /// directory that removes itself.
        struct Fixture {
            root: PathBuf,
        }

        impl Fixture {
            fn new(tag: &str) -> Fixture {
                let root = std::env::temp_dir()
                    .join(format!("zeughaus-update-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&root);
                std::fs::create_dir_all(root.join("bin")).unwrap();
                std::fs::write(root.join("bin/zeughaus-runner"), "old runner").unwrap();
                std::fs::write(root.join("bin/zeughaus"), "old editor").unwrap();
                let root = std::fs::canonicalize(&root).unwrap();
                Fixture { root }
            }

            fn path(&self, relative: &str) -> PathBuf {
                self.root.join(relative)
            }

            fn text(&self, relative: &str) -> String {
                std::fs::read_to_string(self.path(relative)).unwrap()
            }

            fn state_dir(&self) -> PathBuf {
                self.path("state")
            }

            fn state(&self) -> State {
                state::load(&self.state_dir()).unwrap().unwrap()
            }

            /// What a platform job outputs: `zeughaus-<v>-<platform>/` with a
            /// runner that says it is build `reports`, an editor and a
            /// licence, packed.
            fn archive(&self, reports: u64) -> PathBuf {
                let stem = manifest::stem(VERSION, PLATFORM);
                let pack = self.path("pack");
                let top = pack.join(&stem);
                std::fs::create_dir_all(&top).unwrap();
                let runner = top.join("zeughaus-runner");
                std::fs::write(
                    &runner,
                    format!(
                        "#!/bin/sh\n# new runner\n[ \"$1\" = version ] && echo '{VERSION} build {reports}'\n"
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
                std::fs::write(top.join("zeughaus"), "new editor").unwrap();
                std::fs::write(top.join("LICENSE-MIT"), "licence").unwrap();
                let archive = self.path(&manifest::archive_name(VERSION, PLATFORM));
                let packed = Command::new("tar")
                    .arg("-czf")
                    .arg(&archive)
                    .arg("-C")
                    .arg(&pack)
                    .arg(&stem)
                    .status()
                    .unwrap();
                assert!(packed.success());
                archive
            }

            /// The manifest of `archive` as build `build` of the channel.
            fn manifest(&self, build: u64, archive: &Path) -> Manifest {
                Manifest {
                    format: manifest::FORMAT,
                    channel: CHANNEL.into(),
                    build,
                    version: VERSION.into(),
                    sha: "0123456789abcdef0123456789abcdef01234567".into(),
                    artifacts: vec![Artifact::of_file(PLATFORM, archive).unwrap()],
                }
            }

            /// Lays `manifest`, signed by `signer`, and `archive` out under the
            /// channel the way the publish job does: `<channel>/<build>/` and
            /// the `latest` file.
            fn publish(&self, build: u64, manifest: &Manifest, signer: &[u8; 32], archive: &Path) {
                let channel = self.path("site").join(CHANNEL);
                let dir = channel.join(build.to_string());
                std::fs::create_dir_all(&dir).unwrap();
                let json = manifest.to_json();
                std::fs::write(dir.join("manifest.json"), &json).unwrap();
                std::fs::write(dir.join("manifest.sig"), manifest::sign(signer, &json)).unwrap();
                std::fs::copy(archive, dir.join(archive.file_name().unwrap())).unwrap();
                std::fs::write(channel.join("latest"), format!("{build}\n")).unwrap();
            }

            /// Build `build` of the channel, correct and signed by the trusted
            /// key.
            fn publish_build(&self, build: u64) -> PathBuf {
                let archive = self.archive(build);
                self.publish(build, &self.manifest(build, &archive), &SEED, &archive);
                archive
            }

            fn updater(&self) -> Updater {
                Updater {
                    config: UpdateConfig {
                        channel: CHANNEL.into(),
                        url: format!("file://{}", self.path("site").display()),
                    },
                    platform: PLATFORM,
                    work: self.path("state/updates"),
                    state_dir: self.state_dir(),
                    exe: self.path("bin/zeughaus-runner"),
                    product: Product::Runner,
                    home: None,
                    keys: vec![manifest::public_key(&SEED)],
                    own: 3,
                    may_restart: Box::new(|| true),
                    request_restart: Box::new(|| {}),
                }
            }

            fn runner_is_new(&self) -> bool {
                self.text("bin/zeughaus-runner").contains("# new runner")
            }

            /// Nothing the update was to replace has changed.
            fn assert_untouched(&self) {
                assert_eq!(self.text("bin/zeughaus-runner"), "old runner");
                assert_eq!(self.text("bin/zeughaus"), "old editor");
                assert!(!self.path("bin/zeughaus-runner.new").exists());
                assert!(!self.path("bin/zeughaus.new").exists());
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        #[test]
        fn a_published_build_replaces_the_runner() {
            let fx = Fixture::new("runner");
            fx.publish_build(7);
            let updater = fx.updater();
            assert_eq!(updater.latest(), Ok(7));

            let installed = updater.install(7).unwrap();
            assert_eq!(installed.version, VERSION);
            assert!(fx.runner_is_new());
            let mode = std::fs::metadata(fx.path("bin/zeughaus-runner"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
            assert!(!fx.path("bin/zeughaus-runner.new").exists());
            // What was downloaded stays until the next update prunes it.
            assert!(fx.path("state/updates/7/manifest.json").is_file());
            assert!(
                fx.path("state/updates/7/staging")
                    .join(manifest::stem(VERSION, PLATFORM))
                    .is_dir()
            );
        }

        #[test]
        fn a_published_build_keeps_the_previous_one() {
            let fx = Fixture::new("previous");
            fx.publish_build(7);
            fx.updater().update(7);
            assert!(fx.runner_is_new());
            assert_eq!(fx.text("bin/zeughaus-runner.previous"), "old runner");
            let state = fx.state();
            assert_eq!((state.installed, state.previous), (7, 3));
            assert_eq!((state.starts, state.confirmed), (0, false));
        }

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn the_editor_beside_the_runner_is_replaced_with_it() {
            let fx = Fixture::new("editor");
            fx.publish_build(7);
            let installed = fx.updater().install(7).unwrap();
            assert_eq!(fx.text("bin/zeughaus"), "new editor");
            assert_eq!(fx.text("bin/zeughaus.previous"), "old editor");
            assert!(fx.runner_is_new());
            assert_eq!(
                installed.editor,
                Some(Editor::Binary(fx.path("bin/zeughaus")))
            );
            assert!(!fx.path("bin/zeughaus.new").exists());

            // Without an editor beside it only the runner is installed.
            let fx = Fixture::new("no-editor");
            std::fs::remove_file(fx.path("bin/zeughaus")).unwrap();
            fx.publish_build(7);
            let installed = fx.updater().install(7).unwrap();
            assert_eq!(installed.editor, None);
            assert!(fx.runner_is_new());
            assert!(!fx.path("bin/zeughaus").exists());
        }

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn three_unconfirmed_starts_roll_back() {
            let fx = Fixture::new("rollback");
            fx.publish_build(7);
            fx.updater().update(7);
            let exe = fx.path("bin/zeughaus-runner");
            let boot = || boot_restore(&fx.state_dir(), &exe, Product::Runner, None, 7);
            for starts in 1..=STARTS {
                assert_eq!(boot(), Ok(false));
                assert_eq!(fx.state().starts, starts);
                assert!(fx.runner_is_new());
            }

            assert_eq!(boot(), Ok(true));
            assert_untouched_after_rollback(&fx);
            let state = fx.state();
            assert_eq!(state.refused, Some(7));
            assert_eq!((state.installed, state.previous), (3, 0));
            assert!(state.confirmed);

            // Build 3 is not on trial: its starts are not counted.
            assert_eq!(
                boot_restore(&fx.state_dir(), &exe, Product::Runner, None, 3),
                Ok(false)
            );
            assert_eq!(fx.state().starts, 0);
        }

        #[cfg(not(target_os = "macos"))]
        fn assert_untouched_after_rollback(fx: &Fixture) {
            assert_eq!(fx.text("bin/zeughaus-runner"), "old runner");
            assert_eq!(fx.text("bin/zeughaus"), "old editor");
            assert!(!fx.path("bin/zeughaus-runner.previous").exists());
            assert!(!fx.path("bin/zeughaus.previous").exists());
        }

        #[cfg(not(target_os = "macos"))]
        #[test]
        fn a_rollback_by_hand_names_the_restart_and_cannot_repeat() {
            let fx = Fixture::new("rollback-command");
            fx.publish_build(7);
            fx.updater().update(7);
            let exe = fx.path("bin/zeughaus-runner");
            let rollback = || command(&["rollback".into()], &fx.state_dir(), &exe, Product::Runner);
            let answer = rollback().unwrap();
            assert!(answer.starts_with("rolled back to build 3;"), "{answer}");
            assert!(answer.ends_with("restart it"), "{answer}");
            assert_untouched_after_rollback(&fx);
            assert_eq!(fx.state().refused, Some(7));

            assert_eq!(rollback(), Err("no previous build to go back to".into()));
            assert!(command(&["sideways".into()], &fx.state_dir(), &exe, Product::Runner).is_err());
        }

        #[test]
        fn a_refused_build_is_not_installed_again() {
            let fx = Fixture::new("refused");
            fx.publish_build(7);
            let refused = State {
                refused: Some(7),
                ..State::default()
            };
            state::save(&fx.state_dir(), &refused).unwrap();
            let updater = fx.updater();
            let (mut attempted, mut unreachable) = (None, false);
            updater.look(&mut attempted, &mut unreachable);
            fx.assert_untouched();
            assert!(!fx.path("state/updates/7").exists());

            // A newer build is tried as any other.
            fx.publish_build(8);
            updater.look(&mut attempted, &mut unreachable);
            assert!(fx.runner_is_new());
            assert_eq!(fx.state().installed, 8);
        }

        #[test]
        fn nothing_is_attempted_while_a_restart_may_not_happen() {
            let fx = Fixture::new("busy");
            fx.publish_build(7);
            let mut updater = fx.updater();
            updater.may_restart = Box::new(|| false);
            let (mut attempted, mut unreachable) = (None, false);
            updater.look(&mut attempted, &mut unreachable);
            fx.assert_untouched();
            assert_eq!(attempted, None);
        }

        #[test]
        fn a_build_that_does_not_report_itself_installs_nothing() {
            let fx = Fixture::new("preflight");
            let archive = fx.archive(6);
            fx.publish(7, &fx.manifest(7, &archive), &SEED, &archive);
            let err = fx.updater().install(7).err().unwrap();
            assert!(
                err.contains("the new zeughaus-runner does not start"),
                "{err}"
            );
            assert!(err.contains("build 6"), "{err}");
            fx.assert_untouched();
        }

        #[test]
        fn a_windows_style_replace_renames_the_running_file_aside() {
            let fx = Fixture::new("aside");
            let target = fx.path("bin/zeughaus-runner");
            let staged = fx.path("bin/zeughaus-runner.new");
            let previous = previous_of(&target);
            std::fs::write(&staged, "new runner").unwrap();
            std::fs::write(&previous, "older runner").unwrap();

            replace_aside(&staged, &target, &previous, false).unwrap();
            assert_eq!(fx.text("bin/zeughaus-runner"), "new runner");
            assert_eq!(fx.text("bin/zeughaus-runner.previous"), "old runner");
            assert!(!staged.exists());

            // Nothing to put in place: both builds stay as they are.
            assert!(replace_aside(&staged, &target, &previous, false).is_err());
            assert_eq!(fx.text("bin/zeughaus-runner"), "new runner");
            assert_eq!(fx.text("bin/zeughaus-runner.previous"), "old runner");

            restore_aside(&target, &previous, false).unwrap();
            assert_eq!(fx.text("bin/zeughaus-runner"), "old runner");
            assert!(!previous.exists());
            assert!(!fx.path("bin/zeughaus-runner.failed").exists());
        }

        #[test]
        fn a_release_signed_by_another_key_installs_nothing() {
            let fx = Fixture::new("foreign");
            let archive = fx.archive(7);
            fx.publish(7, &fx.manifest(7, &archive), &OTHER, &archive);
            let err = fx.updater().install(7).err().unwrap();
            assert!(err.contains("no trusted key"), "{err}");
            fx.assert_untouched();
        }

        #[test]
        fn a_tampered_archive_installs_nothing() {
            let fx = Fixture::new("tampered");
            let archive = fx.publish_build(7);
            let name = archive.file_name().unwrap();
            let published = fx.path("site/dev/7").join(name);

            // Same size, other bytes.
            let mut bytes = std::fs::read(&published).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 1;
            std::fs::write(&published, &bytes).unwrap();
            let err = fx.updater().install(7).err().unwrap();
            assert!(err.contains("sha256"), "{err}");
            fx.assert_untouched();

            // Another size.
            bytes.push(0);
            std::fs::write(&published, &bytes).unwrap();
            let err = fx.updater().install(7).err().unwrap();
            assert!(err.contains("bytes"), "{err}");
            fx.assert_untouched();
        }

        #[test]
        fn a_manifest_that_is_not_for_this_build_and_channel_installs_nothing() {
            let fx = Fixture::new("mismatch");
            let archive = fx.archive(7);
            // An old release served as a new one.
            fx.publish(7, &fx.manifest(5, &archive), &SEED, &archive);
            let err = fx.updater().install(7).err().unwrap();
            assert!(err.contains("build 5"), "{err}");
            fx.assert_untouched();

            let mut other = fx.manifest(7, &archive);
            other.channel = "nightly".into();
            fx.publish(7, &other, &SEED, &archive);
            let err = fx.updater().install(7).err().unwrap();
            assert!(err.contains("channel nightly"), "{err}");
            fx.assert_untouched();
        }

        #[test]
        fn a_release_without_this_platform_installs_nothing() {
            let fx = Fixture::new("platform");
            fx.publish_build(7);
            let mut updater = fx.updater();
            updater.platform = "darwin-arm64";
            let err = updater.install(7).err().unwrap();
            assert!(err.contains("no archive for darwin-arm64"), "{err}");
            fx.assert_untouched();
        }

        #[test]
        fn a_release_that_is_not_there_installs_nothing() {
            let fx = Fixture::new("missing");
            let updater = fx.updater();
            assert!(updater.latest().is_err());
            assert!(updater.install(7).is_err());
            fx.assert_untouched();
        }

        #[test]
        fn a_failed_attempt_leaves_nothing_under_updates() {
            let fx = Fixture::new("failed");
            let archive = fx.archive(7);
            fx.publish(7, &fx.manifest(7, &archive), &OTHER, &archive);
            fx.updater().update(7);
            assert!(!fx.path("state/updates/7").exists());
            fx.assert_untouched();
        }

        #[test]
        fn only_the_installed_and_the_running_build_stay_under_updates() {
            let fx = Fixture::new("prune");
            let updates = fx.path("state/updates");
            for build in ["3", "5", "6", "7"] {
                std::fs::create_dir_all(updates.join(build).join("staging")).unwrap();
            }
            std::fs::write(updates.join("state.json"), "{}").unwrap();

            fx.updater().prune(&[7, 3]);
            assert!(updates.join("7/staging").is_dir());
            assert!(updates.join("3/staging").is_dir());
            assert!(!updates.join("5").exists());
            assert!(!updates.join("6").exists());
            assert!(updates.join("state.json").is_file());
        }

        #[test]
        fn a_replacement_is_all_or_nothing() {
            let fx = Fixture::new("all-or-nothing");
            std::fs::write(fx.path("first-new"), "first").unwrap();
            let replacements = [
                Replacement::new(fx.path("first-new"), fx.path("bin/zeughaus-runner"), false),
                // Nothing to write the second one from.
                Replacement::new(fx.path("no-such-file"), fx.path("bin/zeughaus"), false),
            ];
            let err = replace_all(&replacements).unwrap_err();
            assert!(err.contains("no-such-file"), "{err}");
            fx.assert_untouched();
        }

        #[test]
        fn a_missing_config_means_no_updater_and_a_bad_one_names_its_file() {
            let fx = Fixture::new("config");
            assert!(load(&fx.root).unwrap().is_none());
            std::fs::write(fx.path(CONFIG_FILE), "channel = [").unwrap();
            let err = load(&fx.root).unwrap_err();
            assert!(
                err.starts_with(&fx.path(CONFIG_FILE).display().to_string()),
                "{err}"
            );
            std::fs::write(fx.path(CONFIG_FILE), "channel = \"dev\"\n").unwrap();
            assert_eq!(load(&fx.root).unwrap().unwrap().channel, "dev");
        }
    }
}
