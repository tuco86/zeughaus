//! `install <profile>`: puts an unpacked release on this host as one of the
//! ways a runner is run, and leaves `install.toml` and `update.toml` so the
//! runner updates itself from then on.
//!
//! ```text
//! zeughaus-runner [--state-dir D] install <desktop|macos|service|windows>
//!     [--channel C] [--feed-addr A] [--bin-dir B] [--from DIR] [--no-service]
//! ```
//!
//! A profile is three kinds of steps: the binaries, copied from the release
//! by a rename; the files of the release's `share/`, written where the
//! profile wants them with `@BINDIR@`, `@FEED_ADDR@` and `@LOGS@` filled in;
//! and the service, started or reloaded. Every build the updater installs
//! writes the files again from its own release ([`refresh`]); binaries and
//! service are the updater's own business then.

use std::path::{Path, PathBuf};
use std::process::Command;

use zeughaus_update::Product;
use zeughaus_update::host::{self, FileStep, InstallConfig};

const USAGE: &str = "usage: zeughaus-runner [--state-dir D] install \
<desktop|macos|service|windows> [--channel C] [--feed-addr A] [--bin-dir B] [--from DIR] \
[--no-service]";

/// The systemd user unit of the desktop and the service profile.
const UNIT: &str = "zeughaus-runner.service";

/// The launchd agent of the macos profile.
const LABEL: &str = "net.doodleshnookie.zeughaus-runner";

/// The address atik's runner serves on, which its editors and the CI name.
const MACOS_FEED_ADDR: &str = "10.8.0.7:7443";

/// The scripts of the Windows VM a service runner boots, and their modes.
const VM_FILES: [(&str, u32); 6] = [
    ("vm/win11/vm.sh", 0o755),
    ("vm/win11/autounattend.xml", 0o644),
    ("vm/win11/setup.ps1", 0o644),
    ("vm/win11/firstboot.ps1", 0o644),
    ("vm/win11/toolchain.ps1", 0o644),
    ("vm/win11/prepare.ps1", 0o644),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    /// A Linux user's runner and editor: systemd user unit, desktop entry.
    Desktop,
    /// A macOS user's runner and `Zeughaus.app`: launchd agent.
    Macos,
    /// The runner of a service user (CI jobs, containers, the VM).
    Service,
    /// The Windows runner, started by its boot task.
    Windows,
}

impl Profile {
    fn parse(name: &str) -> Option<Profile> {
        match name {
            "desktop" => Some(Profile::Desktop),
            "macos" => Some(Profile::Macos),
            "service" => Some(Profile::Service),
            "windows" => Some(Profile::Windows),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Profile::Desktop => "desktop",
            Profile::Macos => "macos",
            Profile::Service => "service",
            Profile::Windows => "windows",
        }
    }

    /// Whether this host is one the profile is for.
    fn fits_host(self) -> bool {
        match self {
            Profile::Desktop | Profile::Service => cfg!(target_os = "linux"),
            Profile::Macos => cfg!(target_os = "macos"),
            Profile::Windows => cfg!(windows),
        }
    }
}

/// The directories the environment names, read once so a test can give its
/// own.
struct Env {
    home: PathBuf,
    /// `$XDG_CONFIG_HOME` or `~/.config`.
    config: PathBuf,
    /// `$XDG_DATA_HOME` or `~/.local/share`.
    data: PathBuf,
    /// `$CARGO_HOME`, where the desktop and macos binaries go by default.
    cargo_home: Option<PathBuf>,
}

impl Env {
    fn from_process() -> Result<Env, String> {
        let home = host::home_dir()?;
        Ok(Env {
            config: host::config_home(&home),
            data: host::data_home(&home),
            cargo_home: std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .filter(|dir| dir.is_absolute()),
            home,
        })
    }
}

/// Where a profile's files go.
struct Dirs<'a> {
    env: &'a Env,
    state: &'a Path,
    bin: &'a Path,
}

impl Dirs<'_> {
    fn unit(&self) -> PathBuf {
        self.env.config.join("systemd/user").join(UNIT)
    }

    fn plist(&self) -> PathBuf {
        self.env
            .home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    }

    fn logs(&self) -> PathBuf {
        self.env.home.join("Library/Logs")
    }

    fn desktop_entry(&self) -> PathBuf {
        self.env
            .data
            .join("applications/net.doodleshnookie.Zeughaus.desktop")
    }

    fn icon(&self) -> PathBuf {
        self.env
            .data
            .join("icons/hicolor/scalable/apps/net.doodleshnookie.Zeughaus.svg")
    }

    fn app(&self) -> PathBuf {
        self.env.home.join("Applications/Zeughaus.app")
    }
}

/// The files of the release's `share/` a profile writes, and where.
fn file_steps(profile: Profile, dirs: &Dirs) -> Vec<FileStep> {
    let step = |share, to, mode| FileStep { share, to, mode };
    match profile {
        Profile::Desktop => vec![
            step("deploy/zeughaus-runner.service", dirs.unit(), 0o644),
            step(
                "deploy/net.doodleshnookie.Zeughaus.desktop",
                dirs.desktop_entry(),
                0o644,
            ),
            step("zeughaus/assets/icon/zeughaus.svg", dirs.icon(), 0o644),
        ],
        Profile::Macos => vec![step(
            "deploy/macos/net.doodleshnookie.zeughaus-runner.plist",
            dirs.plist(),
            0o644,
        )],
        Profile::Service => {
            let mut steps: Vec<FileStep> = VM_FILES
                .iter()
                .map(|&(share, mode)| step(share, dirs.env.home.join("lib").join(share), mode))
                .collect();
            steps.push(step(
                "deploy/runner/ssh_config",
                dirs.env.home.join(".ssh/config"),
                0o600,
            ));
            steps.push(step(
                "deploy/runner/zeughaus-runner.service",
                dirs.unit(),
                0o644,
            ));
            steps
        }
        Profile::Windows => vec![step(
            "deploy/windows/maintain.toml",
            dirs.state.join("maintain.toml"),
            0o644,
        )],
    }
}

/// What the placeholders in a profile's files stand for.
fn values(dirs: &Dirs, feed_addr: Option<&str>) -> Vec<(&'static str, String)> {
    let mut values = vec![
        ("@BINDIR@", dirs.bin.display().to_string()),
        ("@LOGS@", dirs.logs().display().to_string()),
    ];
    if let Some(addr) = feed_addr {
        values.push(("@FEED_ADDR@", addr.to_owned()));
    }
    values
}

/// Reads and renders a profile's files without writing them.
fn render(
    profile: Profile,
    dirs: &Dirs,
    feed_addr: Option<&str>,
    from: &Path,
) -> Result<Vec<host::Rendered>, String> {
    let values = values(dirs, feed_addr);
    let pairs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
    host::render_files(from, &file_steps(profile, dirs), &pairs)
}

#[derive(Debug, PartialEq, Eq)]
struct Request {
    profile: Profile,
    channel: String,
    feed_addr: Option<String>,
    bin_dir: Option<PathBuf>,
    from: Option<PathBuf>,
    /// Leaves the service alone: for a caller that controls it itself.
    no_service: bool,
}

fn parse(args: &[String]) -> Result<Request, String> {
    let (name, rest) = args.split_first().ok_or_else(|| USAGE.to_owned())?;
    let profile = Profile::parse(name).ok_or_else(|| format!("no profile {name:?}\n{USAGE}"))?;
    let mut request = Request {
        profile,
        channel: "dev".to_owned(),
        feed_addr: None,
        bin_dir: None,
        from: None,
        no_service: false,
    };
    let mut rest = rest.iter();
    while let Some(flag) = rest.next() {
        if flag == "--no-service" {
            request.no_service = true;
            continue;
        }
        let value = rest
            .next()
            .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))?;
        match flag.as_str() {
            "--channel" => request.channel = value.clone(),
            "--feed-addr" => request.feed_addr = Some(value.clone()),
            "--bin-dir" => request.bin_dir = Some(PathBuf::from(value)),
            "--from" => request.from = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown flag {flag}\n{USAGE}")),
        }
    }
    Ok(request)
}

/// `install`, from the command line: answers the line to print.
pub fn run(args: &[String], state_dir: &Path) -> Result<String, String> {
    let request = parse(args)?;
    if !request.profile.fits_host() {
        return Err(format!(
            "the {} profile is not for {}",
            request.profile.name(),
            std::env::consts::OS
        ));
    }
    let from = match request.from.clone() {
        Some(from) => from,
        None => std::env::current_exe()
            .map_err(|e| format!("this executable cannot be located: {e}"))?
            .parent()
            .map(Path::to_path_buf)
            .ok_or("this executable has no directory")?,
    };
    install(&request, state_dir, &from, &Env::from_process()?)
}

fn install(request: &Request, state_dir: &Path, from: &Path, env: &Env) -> Result<String, String> {
    let profile = request.profile;
    let feed_addr = match (profile, &request.feed_addr) {
        (Profile::Service, None) => {
            return Err("install service needs --feed-addr <host:port>".into());
        }
        (Profile::Macos, None) => Some(MACOS_FEED_ADDR.to_owned()),
        (Profile::Desktop | Profile::Windows, Some(_)) => {
            return Err(format!("install {} takes no --feed-addr", profile.name()));
        }
        (_, addr) => addr.clone(),
    };
    let bin = match &request.bin_dir {
        Some(bin) => bin.clone(),
        None => match profile {
            Profile::Desktop | Profile::Macos => env
                .cargo_home
                .clone()
                .unwrap_or_else(|| env.home.join(".cargo"))
                .join("bin"),
            Profile::Service => env.home.join("bin"),
            Profile::Windows => return Err("install windows needs --bin-dir <dir>".into()),
        },
    };
    std::fs::create_dir_all(&bin).map_err(|e| format!("cannot create {}: {e}", bin.display()))?;
    let bin = std::path::absolute(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let dirs = Dirs {
        env,
        state: state_dir,
        bin: &bin,
    };

    // Everything is read before anything is written: a release that lacks a
    // file or a binary that does not run changes nothing.
    let files = render(profile, &dirs, feed_addr.as_deref(), from)?;
    let build = host::release_build(from, Product::Runner)?;

    let editor = format!("zeughaus{}", std::env::consts::EXE_SUFFIX);
    let runner = Product::Runner.file_name();
    host::install_binary(&from.join(&runner), &bin.join(&runner))?;
    match profile {
        Profile::Desktop | Profile::Windows => {
            host::install_binary(&from.join(&editor), &bin.join(&editor))?;
        }
        Profile::Macos => host::install_bundle(&from.join("Zeughaus.app"), &dirs.app())?,
        Profile::Service => {}
    }
    let changed = host::write_rendered(&files)?;
    after_files(profile, &dirs, &changed, true);
    let config = InstallConfig {
        profile: profile.name().to_owned(),
        bin_dir: bin.clone(),
        feed_addr,
    };
    host::record(state_dir, &config, &request.channel)?;
    if !request.no_service {
        service(profile, &dirs)?;
    }
    Ok(format!("[install] {} from build {build}", profile.name()))
}

/// What a written file needs besides itself. `always` on an install, where
/// it is cheap to make sure; a refresh only reacts to what changed.
fn after_files(profile: Profile, dirs: &Dirs, changed: &[PathBuf], always: bool) {
    match profile {
        Profile::Desktop => {
            let menu = always
                || changed
                    .iter()
                    .any(|path| *path == dirs.desktop_entry() || *path == dirs.icon());
            if menu {
                refresh_menu(&dirs.env.data);
            }
        }
        Profile::Macos => {
            let logs = dirs.logs();
            if let Err(e) = std::fs::create_dir_all(&logs) {
                eprintln!("[install] cannot create {}: {e}", logs.display());
            }
        }
        Profile::Service | Profile::Windows => {}
    }
}

/// A cache some other installer left in the user's hicolor theme is trusted
/// by Qt as long as it is valid, and it does not list this icon: the menu and
/// the task bar then show a blank one. It is refreshed, never created; KDE's
/// menu is rebuilt where KDE is.
fn refresh_menu(data: &Path) {
    let hicolor = data.join("icons/hicolor");
    if hicolor.join("icon-theme.cache").is_file() {
        quiet(
            Command::new("gtk-update-icon-cache")
                .args(["-f", "-t", "-q"])
                .arg(&hicolor),
            "gtk-update-icon-cache failed",
        );
    }
    quiet(
        &mut Command::new("kbuildsycoca6"),
        "kbuildsycoca6 failed; the menu updates on next login",
    );
}

/// Runs `command` for its effect: a program that is not there is fine, one
/// that fails is said.
fn quiet(command: &mut Command, failed: &str) {
    match command.output() {
        Ok(output) if output.status.success() => {}
        Ok(_) => eprintln!("[install] {failed}"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => eprintln!("[install] {failed}: {e}"),
    }
}

/// Starts the profile's service, or reloads it into the binary just
/// installed, and restarts the editors that run the old one.
#[cfg(unix)]
fn service(profile: Profile, dirs: &Dirs) -> Result<(), String> {
    match profile {
        Profile::Desktop | Profile::Service => {
            let done = host::start_or(UNIT, "reload")?;
            eprintln!("[install] {done} {UNIT}");
            if profile == Profile::Desktop {
                zeughaus_update::editor::restart_binary_editors(&dirs.bin.join("zeughaus"));
            }
        }
        Profile::Macos => {
            // SAFETY: getuid cannot fail.
            let domain = format!("gui/{}", unsafe { libc::getuid() });
            let target = format!("{domain}/{LABEL}");
            let loaded = Command::new("launchctl")
                .args(["print", &target])
                .output()
                .is_ok_and(|output| output.status.success());
            let mut launchctl = Command::new("launchctl");
            if loaded {
                launchctl.args(["kill", "SIGUSR1", &target]);
            } else {
                launchctl.args(["bootstrap", &domain]).arg(dirs.plist());
            }
            let output = launchctl
                .output()
                .map_err(|e| format!("cannot run launchctl: {e}"))?;
            if !output.status.success() {
                return Err(format!(
                    "launchctl: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            eprintln!(
                "[install] {} {LABEL}",
                if loaded { "reloaded" } else { "started" }
            );
            zeughaus_update::editor::restart_app_editors(&dirs.app());
        }
        // The boot task is stopped and started by the bootstrap script.
        Profile::Windows => {}
    }
    Ok(())
}

/// The Windows profile's boot task is stopped and started by the bootstrap
/// script; the others are refused on this host before anything runs.
#[cfg(not(unix))]
fn service(_: Profile, _: &Dirs) -> Result<(), String> {
    Ok(())
}

/// Writes the files of the profile `install.toml` names from the release at
/// `from`: what a new build does once, through the updater's refresh hook.
/// Binaries and service are left alone; a changed unit applies at the next
/// start of the service.
pub fn refresh(state_dir: &Path, from: &Path) {
    match Env::from_process() {
        Ok(env) => refresh_with(state_dir, from, &env),
        Err(e) => eprintln!("[install] {e}"),
    }
}

fn refresh_with(state_dir: &Path, from: &Path, env: &Env) {
    let config = match host::load(state_dir) {
        Ok(Some(config)) => config,
        Ok(None) => return,
        Err(e) => {
            eprintln!("[install] {e}");
            return;
        }
    };
    let Some(profile) = Profile::parse(&config.profile) else {
        eprintln!(
            "[install] {}: {:?} is no runner profile",
            host::INSTALL_FILE,
            config.profile
        );
        return;
    };
    let dirs = Dirs {
        env,
        state: state_dir,
        bin: &config.bin_dir,
    };
    let changed = match render(profile, &dirs, config.feed_addr.as_deref(), from)
        .and_then(|files| host::write_rendered(&files))
    {
        Ok(changed) => changed,
        Err(e) => {
            eprintln!("[install] {e}");
            return;
        }
    };
    for path in &changed {
        eprintln!("[install] wrote {}", path.display());
    }
    after_files(profile, &dirs, &changed, false);
    let unit_changed = changed
        .iter()
        .any(|path| *path == dirs.unit() || *path == dirs.plist());
    if unit_changed {
        if matches!(profile, Profile::Desktop | Profile::Service)
            && let Err(e) = host::systemctl(&["daemon-reload"])
        {
            eprintln!("[install] {e}");
        }
        eprintln!("[install] unit changed; it applies at the next start of the service");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            home: PathBuf::from("/var/lib/zeughaus"),
            config: PathBuf::from("/var/lib/zeughaus/.config"),
            data: PathBuf::from("/var/lib/zeughaus/.local/share"),
            cargo_home: None,
        }
    }

    #[test]
    fn the_service_profile_puts_its_files_under_the_service_home() {
        let env = env();
        let dirs = Dirs {
            env: &env,
            state: Path::new("/var/lib/zeughaus/state"),
            bin: Path::new("/var/lib/zeughaus/bin"),
        };
        let steps = file_steps(Profile::Service, &dirs);
        let find = |share: &str| steps.iter().find(|step| step.share == share).unwrap();
        let vm = find("vm/win11/vm.sh");
        assert_eq!(vm.to, Path::new("/var/lib/zeughaus/lib/vm/win11/vm.sh"));
        assert_eq!(vm.mode, 0o755);
        assert_eq!(find("vm/win11/setup.ps1").mode, 0o644);
        let ssh = find("deploy/runner/ssh_config");
        assert_eq!(
            (ssh.to.as_path(), ssh.mode),
            (Path::new("/var/lib/zeughaus/.ssh/config"), 0o600)
        );
        assert_eq!(
            find("deploy/runner/zeughaus-runner.service").to,
            Path::new("/var/lib/zeughaus/.config/systemd/user/zeughaus-runner.service")
        );
    }

    /// Unix paths: the profiles whose files carry them are unix ones.
    #[cfg(unix)]
    #[test]
    fn placeholders_are_filled_in_from_the_profile() {
        let env = Env {
            home: PathBuf::from("/Users/u"),
            ..env()
        };
        let dirs = Dirs {
            env: &env,
            state: Path::new("/Users/u/.local/state/zeughaus"),
            bin: Path::new("/Users/u/.cargo/bin"),
        };
        let values = values(&dirs, Some("10.8.0.7:7443"));
        let pairs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let plist = "<string>@BINDIR@/zeughaus-runner</string><string>@FEED_ADDR@</string>\
                     <string>@LOGS@/zeughaus-runner.log</string><string>@BINDIR@:/usr/bin</string>";
        assert_eq!(
            host::render(plist, &pairs),
            "<string>/Users/u/.cargo/bin/zeughaus-runner</string><string>10.8.0.7:7443</string>\
             <string>/Users/u/Library/Logs/zeughaus-runner.log</string>\
             <string>/Users/u/.cargo/bin:/usr/bin</string>"
        );
        // Without a feed address its placeholder stays, visibly.
        let values = super::values(&dirs, None);
        let pairs: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_eq!(host::render("@FEED_ADDR@", &pairs), "@FEED_ADDR@");
    }

    #[test]
    fn the_command_line_names_a_profile_and_its_flags() {
        let args = |s: &str| s.split(' ').map(str::to_owned).collect::<Vec<_>>();
        let request = parse(&args("service --feed-addr 10.8.0.1:7443 --channel nightly")).unwrap();
        assert_eq!(request.profile, Profile::Service);
        assert_eq!(request.feed_addr.as_deref(), Some("10.8.0.1:7443"));
        assert_eq!(request.channel, "nightly");
        assert!(!request.no_service);
        assert_eq!(parse(&args("desktop")).unwrap().channel, "dev");
        assert!(parse(&args("desktop --no-service")).unwrap().no_service);
        assert!(parse(&args("laptop")).is_err());
        assert!(parse(&args("desktop --channel")).is_err());
        assert!(parse(&args("desktop --colour red")).is_err());
        assert!(parse(&[]).is_err());
    }

    /// Tests that write files: the integration tier.
    #[cfg(target_os = "linux")]
    mod io {
        use std::os::unix::fs::PermissionsExt;

        use super::*;

        /// An unpacked release and a home in one temporary directory.
        struct Fixture {
            root: PathBuf,
        }

        impl Fixture {
            fn new(tag: &str) -> Fixture {
                let root = std::env::temp_dir()
                    .join(format!("zeughaus-install-{tag}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&root);
                let release = root.join("release");
                std::fs::create_dir_all(&release).unwrap();
                let runner = release.join("zeughaus-runner");
                std::fs::write(
                    &runner,
                    "#!/bin/sh\n[ \"$1\" = version ] && echo '1.2.3 build 42'\n",
                )
                .unwrap();
                std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
                let fx = Fixture { root };
                for (share, _) in VM_FILES {
                    fx.share(share, &format!("{share} body"));
                }
                fx.share("deploy/runner/ssh_config", "Host win11\n");
                fx.share(
                    "deploy/runner/zeughaus-runner.service",
                    "ExecStart=@BINDIR@/zeughaus-runner --feed-addr @FEED_ADDR@\n",
                );
                fx
            }

            fn share(&self, path: &str, text: &str) {
                let path = self.root.join("release/share").join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }

            fn env(&self) -> Env {
                let home = self.root.join("home");
                Env {
                    config: home.join(".config"),
                    data: home.join(".local/share"),
                    cargo_home: None,
                    home,
                }
            }

            fn text(&self, relative: &str) -> String {
                std::fs::read_to_string(self.root.join(relative)).unwrap()
            }

            fn mode(&self, relative: &str) -> u32 {
                std::fs::metadata(self.root.join(relative))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777
            }

            fn install(&self, args: &str) -> Result<String, String> {
                let args: Vec<String> = args.split(' ').map(str::to_owned).collect();
                install(
                    &parse(&args).unwrap(),
                    &self.root.join("home/state"),
                    &self.root.join("release"),
                    &self.env(),
                )
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.root);
            }
        }

        #[test]
        fn the_service_profile_writes_every_file_with_its_mode() {
            let fx = Fixture::new("service");
            let answer = fx
                .install("service --feed-addr 10.8.0.10:7444 --no-service")
                .unwrap();
            assert_eq!(answer, "[install] service from build 42");

            assert!(fx.text("home/bin/zeughaus-runner").contains("build 42"));
            assert_eq!(fx.mode("home/bin/zeughaus-runner"), 0o755);
            assert_eq!(fx.text("home/lib/vm/win11/vm.sh"), "vm/win11/vm.sh body");
            assert_eq!(fx.mode("home/lib/vm/win11/vm.sh"), 0o755);
            assert_eq!(fx.mode("home/lib/vm/win11/autounattend.xml"), 0o644);
            assert_eq!(fx.text("home/.ssh/config"), "Host win11\n");
            assert_eq!(fx.mode("home/.ssh/config"), 0o600);
            let bin = fx.root.join("home/bin");
            assert_eq!(
                fx.text("home/.config/systemd/user/zeughaus-runner.service"),
                format!(
                    "ExecStart={}/zeughaus-runner --feed-addr 10.8.0.10:7444\n",
                    bin.display()
                )
            );
            assert_eq!(fx.text("home/state/update.toml"), "channel = \"dev\"\n");
            let config = host::load(&fx.root.join("home/state")).unwrap().unwrap();
            assert_eq!(config.profile, "service");
            assert_eq!(config.bin_dir, bin);
            assert_eq!(config.feed_addr.as_deref(), Some("10.8.0.10:7444"));
        }

        #[test]
        fn a_release_without_a_file_installs_nothing() {
            let fx = Fixture::new("incomplete");
            std::fs::remove_file(fx.root.join("release/share/deploy/runner/ssh_config")).unwrap();
            let err = fx
                .install("service --feed-addr 10.8.0.10:7444 --no-service")
                .unwrap_err();
            assert_eq!(err, "the release has no share/deploy/runner/ssh_config");
            assert!(!fx.root.join("home/bin/zeughaus-runner").exists());
            assert!(!fx.root.join("home/state/install.toml").exists());

            assert!(fx.install("service --no-service").is_err());
        }

        #[test]
        fn a_new_build_rewrites_the_files_of_the_installed_profile() {
            let fx = Fixture::new("refresh");
            fx.install("service --feed-addr 10.8.0.10:7444 --no-service")
                .unwrap();
            fx.share("deploy/runner/ssh_config", "Host win11\n    Port 2222\n");
            refresh_with(
                &fx.root.join("home/state"),
                &fx.root.join("release"),
                &fx.env(),
            );
            assert_eq!(fx.text("home/.ssh/config"), "Host win11\n    Port 2222\n");
            assert_eq!(fx.mode("home/.ssh/config"), 0o600);
        }
    }
}
