//! The self-updater: a process that follows a release channel and installs
//! what the CI publishes there.
//!
//! `<state-dir>/update.toml` turns it on; without the file nothing runs:
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
//! the size and SHA-256 the manifest gives. The runner is replaced by a rename
//! of a file written next to it, and so is the editor installed beside it
//! (`zeughaus` in the runner's directory, or `~/Applications/Zeughaus.app` on
//! macOS), so the two always run one build. Running editors are then asked to
//! restart by `SIGUSR1`, which they answer by reopening as they were, and this
//! process restarts the way its own `SIGUSR1` makes it.
//!
//! `zeughaus-ci` is never installed here: the CI stays on explicit deploys, so
//! a broken build cannot take down the service that would publish its fix.
//!
//! [`manifest`] is built on every platform; the updater only on unix, where
//! there is a signal to restart with.

pub mod manifest;

#[cfg(unix)]
pub use self::unix::start;

/// `[a-z0-9][a-z0-9-]*`: a channel names a directory under the release URL.
fn valid_channel(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::time::Duration;

    use serde::Deserialize;

    use super::manifest;
    use crate::valid_channel;

    /// `<state-dir>/update.toml`.
    const CONFIG_FILE: &str = "update.toml";

    /// Where the zeughaus CI publishes its channels.
    const DEFAULT_URL: &str = "https://zeughaus.doodleshnookie.net/updates";

    /// The wait before the first look, so that a runner that has just started,
    /// or restarted into an update, is serving before it begins a download.
    const FIRST_LOOK: Duration = Duration::from_secs(10);

    /// The time between two looks at the channel.
    const INTERVAL: Duration = Duration::from_secs(60);

    /// What the `latest` file, a few bytes, may take.
    const LATEST_SECONDS: &str = "20";

    /// What a manifest or an archive may take; the archive is a whole build.
    const DOWNLOAD_SECONDS: &str = "600";

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

    /// `Ok(None)` when there is no `update.toml`: this runner does not update.
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

    /// Starts the updater if `<state-dir>/update.toml` asks for one. `exe` is
    /// this binary's path as resolved at start, the file an update replaces;
    /// `request_restart` asks the process to restart into it.
    pub fn start(state_dir: &Path, exe: PathBuf, request_restart: Box<dyn Fn() + Send>) {
        let config = match load(state_dir) {
            Ok(Some(config)) => config,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[update] {e}");
                return;
            }
        };
        let Some(platform) = manifest::host_platform() else {
            eprintln!(
                "[update] no release is built for {} on {}; not updating",
                std::env::consts::OS,
                std::env::consts::ARCH
            );
            return;
        };
        let updater = Updater {
            platform,
            work: state_dir.join("updates"),
            exe,
            home: std::env::var_os("HOME").map(PathBuf::from),
            keys: manifest::TRUSTED_KEYS.to_vec(),
            own: manifest::own_build(),
            config,
            request_restart,
        };
        eprintln!(
            "[update] following {} at {}, running build {}",
            updater.config.channel, updater.config.url, updater.own
        );
        let spawned = std::thread::Builder::new()
            .name("zeughaus-update".into())
            .spawn(move || updater.run());
        if let Err(e) = spawned {
            eprintln!("[update] cannot start: {e}");
        }
    }

    struct Updater {
        config: UpdateConfig,
        /// What the release calls this host.
        platform: &'static str,
        /// `<state-dir>/updates`: one directory per build being installed.
        work: PathBuf,
        /// The runner's own binary, the file an update replaces.
        exe: PathBuf,
        /// Where `Zeughaus.app` is looked for on macOS.
        home: Option<PathBuf>,
        /// The keys a manifest may be signed by.
        keys: Vec<[u8; 32]>,
        /// The build this binary was made by; 0 when the CI did not make it.
        own: u64,
        /// Asks the process to restart into the build just installed.
        request_restart: Box<dyn Fn() + Send>,
    }

    impl Updater {
        fn run(self) {
            std::thread::sleep(FIRST_LOOK);
            // The build the last attempt was for, installed or not: a restart
            // that fails to exec leaves this image running at its old build
            // number, and a build that failed would fail the same way a
            // minute later.
            let mut attempted = None;
            // Said once per outage, not once a minute.
            let mut unreachable = false;
            loop {
                match self.latest() {
                    Ok(latest) => {
                        unreachable = false;
                        if latest > self.own && attempted != Some(latest) {
                            attempted = Some(latest);
                            self.update(latest);
                        }
                    }
                    Err(e) => {
                        if !unreachable {
                            eprintln!("[update] {e}");
                        }
                        unreachable = true;
                    }
                }
                std::thread::sleep(INTERVAL);
            }
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

        /// One attempt at `build`, to its end: installed and the restarts
        /// asked for, or logged and removed.
        fn update(&self, build: u64) {
            match self.install(build) {
                Ok(installed) => {
                    self.prune(build);
                    eprintln!(
                        "[update] installed build {build} ({}); restarting",
                        installed.version
                    );
                    if let Some(editor) = &installed.editor {
                        editor.restart();
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

            let editor = Editor::find(&self.exe, self.home.as_deref());
            let mut replacements = vec![Replacement::new(
                top.join("zeughaus-runner"),
                self.exe.clone(),
                false,
            )];
            if let Some(editor) = &editor {
                replacements.push(editor.replacement(&top));
            }
            replace_all(&replacements)?;
            Ok(Installed {
                version: manifest.version,
                editor,
            })
        }

        /// Removes every directory under `updates/` but `keep`'s: the builds
        /// that failed and the ones this one replaces.
        fn prune(&self, keep: u64) {
            let keep = keep.to_string();
            let Ok(entries) = std::fs::read_dir(&self.work) else {
                return;
            };
            for entry in entries.flatten() {
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                if is_dir && entry.file_name() != keep.as_str() {
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

    /// The editor installed beside the runner. It is replaced with it so that
    /// the two ends of the link always run the same build.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Editor {
        /// `zeughaus` in the runner's directory.
        Binary(PathBuf),
        /// `~/Applications/Zeughaus.app`, where `deploy/macos-app.sh` puts it.
        App(PathBuf),
    }

    impl Editor {
        fn find(exe: &Path, home: Option<&Path>) -> Option<Editor> {
            if cfg!(target_os = "macos") {
                let app = home?.join("Applications/Zeughaus.app");
                app.is_dir().then_some(Editor::App(app))
            } else {
                let binary = exe.with_file_name("zeughaus");
                binary.is_file().then_some(Editor::Binary(binary))
            }
        }

        /// This editor's replacement from the unpacked release at `top`.
        fn replacement(&self, top: &Path) -> Replacement {
            match self {
                Editor::Binary(path) => Replacement::new(top.join("zeughaus"), path.clone(), false),
                Editor::App(app) => Replacement::new(top.join("Zeughaus.app"), app.clone(), true),
            }
        }

        /// Asks every running copy to restart into what was installed.
        fn restart(&self) {
            match self {
                Editor::Binary(path) => restart_binary_editors(path),
                Editor::App(app) => restart_app_editors(app),
            }
        }
    }

    /// A file or an app bundle that is replaced in two steps: written next to
    /// its target, then put over it by a rename. The first step can fail for
    /// a full disk or a directory that is not writable, the second hardly
    /// ever, so `replace_all` does the first for everything before the second
    /// for anything.
    struct Replacement {
        from: PathBuf,
        target: PathBuf,
        /// `<target>.new`, on the target's file system, where a rename works.
        staged: PathBuf,
        /// A directory tree (an app bundle) and not a file.
        bundle: bool,
    }

    impl Replacement {
        fn new(from: PathBuf, target: PathBuf, bundle: bool) -> Replacement {
            let staged = sibling(&target, ".new");
            Replacement {
                from,
                target,
                staged,
                bundle,
            }
        }

        fn stage(&self) -> Result<(), String> {
            if !self.from.exists() {
                let name = self.from.file_name().unwrap_or_default();
                return Err(format!("the archive has no {}", name.to_string_lossy()));
            }
            if self.bundle {
                // `ditto` keeps what an app bundle is made of: links, modes
                // and the code signature.
                let _ = std::fs::remove_dir_all(&self.staged);
                let output = Command::new("ditto")
                    .arg(&self.from)
                    .arg(&self.staged)
                    .output()
                    .map_err(|e| format!("cannot run ditto: {e}"))?;
                if !output.status.success() {
                    return Err(format!("ditto: {}", stderr_of(&output)));
                }
            } else {
                std::fs::copy(&self.from, &self.staged)
                    .map_err(|e| format!("cannot write {}: {e}", self.staged.display()))?;
                std::fs::set_permissions(&self.staged, Permissions::from_mode(0o755))
                    .map_err(|e| format!("cannot chmod {}: {e}", self.staged.display()))?;
            }
            Ok(())
        }

        fn commit(&self) -> Result<(), String> {
            if !self.bundle {
                return std::fs::rename(&self.staged, &self.target)
                    .map_err(|e| format!("cannot replace {}: {e}", self.target.display()));
            }
            let old = sibling(&self.target, ".old");
            let _ = std::fs::remove_dir_all(&old);
            std::fs::rename(&self.target, &old)
                .map_err(|e| format!("cannot move {} aside: {e}", self.target.display()))?;
            if let Err(e) = std::fs::rename(&self.staged, &self.target) {
                // No editor at all is worse than an old one.
                let _ = std::fs::rename(&old, &self.target);
                return Err(format!("cannot replace {}: {e}", self.target.display()));
            }
            let _ = std::fs::remove_dir_all(&old);
            Ok(())
        }

        /// Removes what was written and not renamed into place.
        fn discard(&self) {
            if self.bundle {
                let _ = std::fs::remove_dir_all(&self.staged);
            } else {
                let _ = std::fs::remove_file(&self.staged);
            }
        }
    }

    /// Writes every replacement, then puts them in place.
    fn replace_all(replacements: &[Replacement]) -> Result<(), String> {
        let replaced = replacements
            .iter()
            .try_for_each(Replacement::stage)
            .and_then(|()| replacements.iter().try_for_each(Replacement::commit));
        replacements.iter().for_each(Replacement::discard);
        replaced
    }

    /// `path` with `suffix` appended to its file name.
    fn sibling(path: &Path, suffix: &str) -> PathBuf {
        let mut name: OsString = path.as_os_str().to_owned();
        name.push(suffix);
        PathBuf::from(name)
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

    /// Unpacks the gzipped tar `archive` into `into`.
    fn extract(archive: &Path, into: &Path) -> Result<(), String> {
        std::fs::create_dir_all(into)
            .map_err(|e| format!("cannot create {}: {e}", into.display()))?;
        let output = Command::new("tar")
            .arg("-xzf")
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

    /// Whether `exe`, what `/proc/<pid>/exe` links to, is the file `editor`,
    /// or was: the kernel names the executable of a process whose file was
    /// replaced `<path> (deleted)`.
    fn runs_file(exe: &Path, editor: &Path) -> bool {
        let mut replaced = editor.as_os_str().to_owned();
        replaced.push(" (deleted)");
        exe == editor || exe.as_os_str() == replaced
    }

    /// Whether a `/proc/<pid>/status` says the process catches `SIGUSR1`.
    /// One that does not would be ended by the signal, not restarted.
    fn catches_usr1(status: &str) -> bool {
        status
            .lines()
            .find_map(|line| line.strip_prefix("SigCgt:"))
            .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
            .is_some_and(|mask| mask & (1u64 << (libc::SIGUSR1 - 1)) != 0)
    }

    /// `SIGUSR1` to every running copy of the editor binary `editor`, which
    /// is what `deploy/install.sh` does after it installed one.
    fn restart_binary_editors(editor: &Path) {
        let Ok(processes) = std::fs::read_dir("/proc") else {
            return;
        };
        for process in processes.flatten() {
            let Some(pid) = process
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            // Another user's process does not show its executable.
            let Ok(exe) = std::fs::read_link(process.path().join("exe")) else {
                continue;
            };
            if !runs_file(&exe, editor) {
                continue;
            }
            let status = std::fs::read_to_string(process.path().join("status")).unwrap_or_default();
            if catches_usr1(&status) {
                restart_editor(pid);
            } else {
                eprintln!("[update] editor {pid} cannot restart itself; close and reopen it");
            }
        }
    }

    /// `SIGUSR1` to every running copy of the editor inside `app`.
    fn restart_app_editors(app: &Path) {
        let pattern = format!("{}/Contents/MacOS/zeughaus", app.display());
        let output = match Command::new("pgrep").arg("-f").arg(&pattern).output() {
            Ok(output) => output,
            Err(e) => {
                eprintln!("[update] cannot run pgrep: {e}");
                return;
            }
        };
        // `pgrep` exits 1 when nothing matches, and prints nothing.
        for pid in String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<libc::pid_t>().ok())
        {
            restart_editor(pid);
        }
    }

    fn restart_editor(pid: libc::pid_t) {
        // SAFETY: `kill` takes two integers; a process that ended meanwhile
        // answers with `ESRCH`.
        if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 {
            eprintln!("[update] restarting editor {pid}");
        } else {
            eprintln!(
                "[update] cannot signal editor {pid}: {}",
                std::io::Error::last_os_error()
            );
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

            let config = UpdateConfig::parse(
                "channel = \"nightly\"\nurl = \"http://localhost:8080/updates/\"",
            )
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

        #[test]
        fn a_replaced_editor_is_still_found() {
            let editor = Path::new("/home/u/.cargo/bin/zeughaus");
            assert!(runs_file(editor, editor));
            assert!(runs_file(
                Path::new("/home/u/.cargo/bin/zeughaus (deleted)"),
                editor
            ));
            assert!(!runs_file(
                Path::new("/home/u/.cargo/bin/zeughaus-runner"),
                editor
            ));
            assert!(!runs_file(Path::new("/usr/bin/zeughaus"), editor));
            assert!(!runs_file(
                Path::new("/home/u/.cargo/bin/zeughaus-runner (deleted)"),
                editor
            ));
        }

        #[test]
        fn only_a_process_that_catches_usr1_is_signalled() {
            let bit = 1u64 << (libc::SIGUSR1 - 1);
            let status = |caught: u64| {
                format!(
                    "Name:\tzeughaus\nSigBlk:\t0000000000000000\nSigIgn:\t0000000000001000\n\
                     SigCgt:\t{caught:016x}\n"
                )
            };
            assert!(catches_usr1(&status(bit)));
            assert!(catches_usr1(&status(bit | 1)));
            assert!(!catches_usr1(&status(0)));
            assert!(!catches_usr1(&status(!bit)));
            assert!(!catches_usr1("Name:\tzeughaus\n"));
            assert!(!catches_usr1("SigCgt:\tnot hex\n"));
        }

        /// Tests that touch the filesystem, network, processes or real time: the
        /// integration tier (`.config/nextest.toml`).
        mod io {
            use super::*;

            use super::manifest::{Artifact, Manifest};

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
                    // `/proc/<pid>/exe` names the resolved path.
                    let root = std::fs::canonicalize(&root).unwrap();
                    Fixture { root }
                }

                fn path(&self, relative: &str) -> PathBuf {
                    self.root.join(relative)
                }

                fn text(&self, relative: &str) -> String {
                    std::fs::read_to_string(self.path(relative)).unwrap()
                }

                /// What a platform job outputs: `zeughaus-<v>-<platform>/` with a
                /// runner, an editor and a licence, packed.
                fn archive(&self) -> PathBuf {
                    let stem = manifest::stem(VERSION, PLATFORM);
                    let pack = self.path("pack");
                    let top = pack.join(&stem);
                    std::fs::create_dir_all(&top).unwrap();
                    std::fs::write(top.join("zeughaus-runner"), "new runner").unwrap();
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

                /// Lays `manifest`, signed by `signer`, and `archive` out under
                /// the channel the way the publish job does: `<channel>/<build>/`
                /// and the `latest` file.
                fn publish(
                    &self,
                    build: u64,
                    manifest: &Manifest,
                    signer: &[u8; 32],
                    archive: &Path,
                ) {
                    let channel = self.path("site").join(CHANNEL);
                    let dir = channel.join(build.to_string());
                    std::fs::create_dir_all(&dir).unwrap();
                    let json = manifest.to_json();
                    std::fs::write(dir.join("manifest.json"), &json).unwrap();
                    std::fs::write(dir.join("manifest.sig"), manifest::sign(signer, &json))
                        .unwrap();
                    std::fs::copy(archive, dir.join(archive.file_name().unwrap())).unwrap();
                    std::fs::write(channel.join("latest"), format!("{build}\n")).unwrap();
                }

                /// Build 7 of the channel, correct and signed by the trusted key.
                fn publish_seven(&self) -> PathBuf {
                    let archive = self.archive();
                    self.publish(7, &self.manifest(7, &archive), &SEED, &archive);
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
                        exe: self.path("bin/zeughaus-runner"),
                        home: None,
                        keys: vec![manifest::public_key(&SEED)],
                        own: 3,
                        request_restart: Box::new(|| {}),
                    }
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
                fx.publish_seven();
                let updater = fx.updater();
                assert_eq!(updater.latest(), Ok(7));

                let installed = updater.install(7).unwrap();
                assert_eq!(installed.version, VERSION);
                assert_eq!(fx.text("bin/zeughaus-runner"), "new runner");
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

            #[cfg(not(target_os = "macos"))]
            #[test]
            fn the_editor_beside_the_runner_is_replaced_with_it() {
                let fx = Fixture::new("editor");
                fx.publish_seven();
                let installed = fx.updater().install(7).unwrap();
                assert_eq!(fx.text("bin/zeughaus"), "new editor");
                assert_eq!(fx.text("bin/zeughaus-runner"), "new runner");
                assert_eq!(
                    installed.editor,
                    Some(Editor::Binary(fx.path("bin/zeughaus")))
                );
                assert!(!fx.path("bin/zeughaus.new").exists());

                // Without an editor beside it only the runner is installed.
                let fx = Fixture::new("no-editor");
                std::fs::remove_file(fx.path("bin/zeughaus")).unwrap();
                fx.publish_seven();
                let installed = fx.updater().install(7).unwrap();
                assert_eq!(installed.editor, None);
                assert_eq!(fx.text("bin/zeughaus-runner"), "new runner");
                assert!(!fx.path("bin/zeughaus").exists());
            }

            #[test]
            fn a_release_signed_by_another_key_installs_nothing() {
                let fx = Fixture::new("foreign");
                let archive = fx.archive();
                fx.publish(7, &fx.manifest(7, &archive), &OTHER, &archive);
                let err = fx.updater().install(7).err().unwrap();
                assert!(err.contains("no trusted key"), "{err}");
                fx.assert_untouched();
            }

            #[test]
            fn a_tampered_archive_installs_nothing() {
                let fx = Fixture::new("tampered");
                let archive = fx.publish_seven();
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
                let archive = fx.archive();
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
                fx.publish_seven();
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
                let archive = fx.archive();
                fx.publish(7, &fx.manifest(7, &archive), &OTHER, &archive);
                fx.updater().update(7);
                assert!(!fx.path("state/updates/7").exists());
                fx.assert_untouched();
            }

            #[test]
            fn only_the_installed_build_stays_under_updates() {
                let fx = Fixture::new("prune");
                let updates = fx.path("state/updates");
                for build in ["5", "6", "7"] {
                    std::fs::create_dir_all(updates.join(build).join("staging")).unwrap();
                }
                std::fs::write(updates.join("stray"), "not a directory").unwrap();

                fx.updater().prune(7);
                assert!(updates.join("7/staging").is_dir());
                assert!(!updates.join("5").exists());
                assert!(!updates.join("6").exists());
                assert!(updates.join("stray").is_file());
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

            /// A copy of the shell at `<root>/editor/zeughaus` running `script`,
            /// then replaced on disk, the way an update replaces an editor: a
            /// process whose executable is a file that is gone. `script` is given
            /// the path of a file to create once it has set itself up.
            #[cfg(target_os = "linux")]
            fn running_editor(fx: &Fixture, script: &str) -> (PathBuf, std::process::Child) {
                let editor = fx.path("editor/zeughaus");
                std::fs::create_dir_all(editor.parent().unwrap()).unwrap();
                std::fs::copy("/bin/sh", &editor).unwrap();
                let ready = fx.path("ready");
                let mut attempts = 0;
                let child = loop {
                    match Command::new(&editor)
                        .args(["-c", script, "sh"])
                        .arg(&ready)
                        .spawn()
                    {
                        // The copy is still open for writing in another test's fork.
                        Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 100 => {
                            attempts += 1;
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        spawned => break spawned.unwrap(),
                    }
                };
                for _ in 0..500 {
                    if ready.exists() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(ready.exists(), "the script did not start");
                std::fs::write(fx.path("editor/zeughaus.new"), "new").unwrap();
                std::fs::rename(fx.path("editor/zeughaus.new"), &editor).unwrap();
                (editor, child)
            }

            #[cfg(target_os = "linux")]
            #[test]
            fn a_running_editor_that_catches_usr1_is_restarted() {
                let fx = Fixture::new("usr1");
                let (editor, mut child) = running_editor(
                    &fx,
                    "trap 'exit 0' USR1; touch \"$1\"; while :; do sleep 0.05; done",
                );
                restart_binary_editors(&editor);
                let ended = (0..500).find_map(|_| {
                    std::thread::sleep(Duration::from_millis(10));
                    child.try_wait().unwrap()
                });
                let _ = child.kill();
                let _ = child.wait();
                assert_eq!(
                    ended.and_then(|status| status.code()),
                    Some(0),
                    "the editor was not signalled"
                );
            }

            #[cfg(target_os = "linux")]
            #[test]
            fn a_running_editor_that_does_not_catch_usr1_is_left_alone() {
                let fx = Fixture::new("no-usr1");
                let (editor, mut child) =
                    running_editor(&fx, "touch \"$1\"; while :; do sleep 0.05; done");
                restart_binary_editors(&editor);
                // The signal's default action would end the shell within moments.
                std::thread::sleep(Duration::from_millis(300));
                let alive = child.try_wait().unwrap().is_none();
                let _ = child.kill();
                let _ = child.wait();
                assert!(
                    alive,
                    "a process that cannot restart itself must not be signalled"
                );
            }
        }
    }
}
